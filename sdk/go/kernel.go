package metera

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/api"
)

const protocol = "metera.kernel.v1"

var (
	ErrInvalidModule = errors.New("metera: invalid wasm module")
	ErrABIMismatch   = errors.New("metera: wasm ABI version mismatch")
	ErrExecution     = errors.New("metera: wasm engine failure")
	ErrBuffer        = errors.New("metera: invalid guest buffer")
	ErrEnvelope      = errors.New("metera: malformed response envelope")
	ErrTooLarge      = errors.New("metera: request too large")
)

var refusalCodes = map[string]bool{
	"invalid_request":       true,
	"protocol_mismatch":     true,
	"unsupported_operation": true,
	"invalid_payload":       true,
	"encoding_failed":       true,

	"usage_declaration_invalid":    true,
	"usage_schema_unsupported":     true,
	"usage_record_invalid":         true,
	"usage_detail_session_missing": true,
	"usage_counter_overflow":       true,
	"usage_reading_incomplete":     true,
	"usage_reading_inconsistent":   true,
	"usage_series_mismatch":        true,
}

type RefusedError struct {
	Code string
}

func (e *RefusedError) Error() string {
	return "metera: kernel refused: " + e.Code
}

func engineFailure(ctx context.Context, fallback error) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	return fallback
}

type Kernel struct {
	runtime wazero.Runtime
	module  api.Module
	memory  api.Memory
	alloc   api.Function
	call    api.Function
	release api.Function
	gate    chan struct{}
}

func signature(fn api.Function, params, results []api.ValueType) bool {
	if fn == nil {
		return false
	}
	definition := fn.Definition()
	return slices.Equal(definition.ParamTypes(), params) && slices.Equal(definition.ResultTypes(), results)
}

func New(ctx context.Context, wasm []byte) (*Kernel, error) {
	if len(wasm) < 4 || string(wasm[:4]) != "\x00asm" {
		return nil, ErrInvalidModule
	}
	runtime := wazero.NewRuntimeWithConfig(ctx, wazero.NewRuntimeConfig().WithCloseOnContextDone(true))
	module, err := runtime.InstantiateWithConfig(ctx, wasm, wazero.NewModuleConfig().WithStartFunctions())
	if err != nil {
		_ = runtime.Close(ctx)
		return nil, engineFailure(ctx, ErrInvalidModule)
	}
	fail := func(err error) (*Kernel, error) {
		_ = runtime.Close(ctx)
		return nil, err
	}
	memory := module.ExportedMemory("memory")
	abi := module.ExportedFunction("sb_abi_version")
	alloc := module.ExportedFunction("sb_alloc")
	call := module.ExportedFunction("sb_call")
	release := module.ExportedFunction("sb_release")
	if memory == nil || abi == nil || alloc == nil || call == nil || release == nil {
		return fail(fmt.Errorf("%w: missing export", ErrInvalidModule))
	}
	if !signature(abi, nil, []api.ValueType{api.ValueTypeI32}) ||
		!signature(alloc, []api.ValueType{api.ValueTypeI32}, []api.ValueType{api.ValueTypeI32}) ||
		!signature(call, []api.ValueType{api.ValueTypeI32, api.ValueTypeI32}, []api.ValueType{api.ValueTypeI64}) ||
		!signature(release, []api.ValueType{api.ValueTypeI32}, nil) {
		return fail(ErrABIMismatch)
	}
	version, err := abi.Call(ctx)
	if err != nil {
		return fail(engineFailure(ctx, ErrExecution))
	}
	if len(version) != 1 || version[0] != 1 {
		return fail(ErrABIMismatch)
	}
	return &Kernel{
		runtime: runtime,
		module:  module,
		memory:  memory,
		alloc:   alloc,
		call:    call,
		release: release,
		gate:    make(chan struct{}, 1),
	}, nil
}

func (k *Kernel) Invoke(ctx context.Context, request []byte) ([]byte, error) {
	select {
	case k.gate <- struct{}{}:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	defer func() { <-k.gate }()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if uint64(len(request)) > uint64(^uint32(0)) {
		return nil, ErrTooLarge
	}
	cleanup := context.WithoutCancel(ctx)
	length := uint64(uint32(len(request)))
	allocated, err := k.alloc.Call(ctx, length)
	if err != nil {
		return nil, engineFailure(ctx, ErrExecution)
	}
	if len(allocated) != 1 {
		return nil, ErrBuffer
	}
	inPtr := uint32(allocated[0])
	if inPtr == 0 {
		return nil, ErrBuffer
	}
	if !k.memory.Write(inPtr, request) {
		_, _ = k.release.Call(cleanup, uint64(inPtr))
		return nil, ErrBuffer
	}
	packed, err := k.call.Call(ctx, uint64(inPtr), length)
	if err != nil {
		_, _ = k.release.Call(cleanup, uint64(inPtr))
		return nil, engineFailure(ctx, ErrExecution)
	}
	if len(packed) != 1 {
		return nil, ErrBuffer
	}
	outPtr := uint32(packed[0])
	outLen := uint32(packed[0] >> 32)
	if outPtr == 0 {
		return nil, ErrBuffer
	}
	data, ok := k.memory.Read(outPtr, outLen)
	if !ok {
		_, _ = k.release.Call(cleanup, uint64(outPtr))
		return nil, ErrBuffer
	}
	out := append([]byte(nil), data...)
	if _, err := k.release.Call(cleanup, uint64(outPtr)); err != nil {
		return nil, engineFailure(ctx, ErrExecution)
	}
	return out, nil
}

func parseResponse(out []byte) (json.RawMessage, error) {
	var body map[string]json.RawMessage
	if err := json.Unmarshal(out, &body); err != nil {
		return nil, ErrEnvelope
	}
	declaredRaw, ok := body["protocol"]
	var declared string
	if !ok || json.Unmarshal(declaredRaw, &declared) != nil || declared != protocol {
		return nil, ErrEnvelope
	}
	result, hasResult := body["result"]
	refusalRaw, hasError := body["error"]
	if hasResult == hasError {
		return nil, ErrEnvelope
	}
	if hasResult {
		return result, nil
	}
	var refusal struct {
		Code json.RawMessage `json:"code"`
	}
	if json.Unmarshal(refusalRaw, &refusal) != nil {
		return nil, ErrEnvelope
	}
	var code string
	if json.Unmarshal(refusal.Code, &code) != nil || !refusalCodes[code] {
		return nil, ErrEnvelope
	}
	return nil, &RefusedError{Code: code}
}

func (k *Kernel) Call(ctx context.Context, operation string, payload json.RawMessage) (json.RawMessage, error) {
	if payload == nil {
		payload = json.RawMessage("null")
	}
	request, err := json.Marshal(struct {
		Protocol  string          `json:"protocol"`
		Operation string          `json:"operation"`
		Payload   json.RawMessage `json:"payload"`
	}{protocol, operation, payload})
	if err != nil {
		return nil, ErrEnvelope
	}
	out, err := k.Invoke(ctx, request)
	if err != nil {
		return nil, err
	}
	return parseResponse(out)
}

func (k *Kernel) Close(ctx context.Context) error {
	if err := k.runtime.Close(ctx); err != nil {
		return engineFailure(ctx, ErrExecution)
	}
	return nil
}
