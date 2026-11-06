package metera

import (
	"context"
	"errors"
	"testing"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/api"
)

const (
	wasmI32 byte = 0x7f
	wasmI64 byte = 0x7e
)

func leb(value uint64) []byte {
	var out []byte
	for {
		b := byte(value & 0x7f)
		value >>= 7
		if value != 0 {
			b |= 0x80
		}
		out = append(out, b)
		if value == 0 {
			return out
		}
	}
}

func sleb(value int64) []byte {
	var out []byte
	for {
		b := byte(value & 0x7f)
		value >>= 7
		if (value == 0 && b&0x40 == 0) || (value == -1 && b&0x40 != 0) {
			return append(out, b)
		}
		out = append(out, b|0x80)
	}
}

func wasmSection(id byte, payload []byte) []byte {
	out := append([]byte{id}, leb(uint64(len(payload)))...)
	return append(out, payload...)
}

func wasmVec(items [][]byte) []byte {
	out := leb(uint64(len(items)))
	for _, item := range items {
		out = append(out, item...)
	}
	return out
}

func wasmName(value string) []byte {
	return append(leb(uint64(len(value))), value...)
}

func wasmFuncType(params, results []byte) []byte {
	out := append([]byte{0x60}, leb(uint64(len(params)))...)
	out = append(out, params...)
	out = append(out, leb(uint64(len(results)))...)
	return append(out, results...)
}

func wasmExport(name string, kind byte, index uint64) []byte {
	out := wasmName(name)
	out = append(out, kind)
	return append(out, leb(index)...)
}

func wasmBody(instructions []byte) []byte {
	return append(leb(uint64(len(instructions)+2)), append([]byte{0x00}, append(instructions, 0x0b)...)...)
}

type moduleSpec struct {
	abiResult   byte
	abiValue    int64
	allocParams []byte
	allocValue  int64
	callValue   int64
	memoryName  string
	extraStart  bool
}

func defaultModule() moduleSpec {
	return moduleSpec{
		abiResult:   wasmI32,
		abiValue:    1,
		allocParams: []byte{wasmI32},
		allocValue:  1,
		callValue:   2<<32 | 1,
		memoryName:  "memory",
	}
}

func buildModule(spec moduleSpec) []byte {
	types := [][]byte{
		wasmFuncType(nil, []byte{spec.abiResult}),
		wasmFuncType(spec.allocParams, []byte{wasmI32}),
		wasmFuncType([]byte{wasmI32, wasmI32}, []byte{wasmI64}),
		wasmFuncType([]byte{wasmI32}, nil),
	}
	funcs := [][]byte{leb(0), leb(1), leb(2), leb(3)}
	abiOp := byte(0x41)
	if spec.abiResult == wasmI64 {
		abiOp = 0x42
	}
	bodies := [][]byte{
		wasmBody(append([]byte{abiOp}, sleb(spec.abiValue)...)),
		wasmBody(append([]byte{0x41}, sleb(spec.allocValue)...)),
		wasmBody(append([]byte{0x42}, sleb(spec.callValue)...)),
		wasmBody(nil),
	}
	exports := [][]byte{
		wasmExport("sb_abi_version", 0x00, 0),
		wasmExport("sb_alloc", 0x00, 1),
		wasmExport("sb_call", 0x00, 2),
		wasmExport("sb_release", 0x00, 3),
		wasmExport(spec.memoryName, 0x02, 0),
	}
	if spec.extraStart {
		types = append(types, wasmFuncType(nil, nil))
		funcs = append(funcs, leb(4))
		bodies = append(bodies, wasmBody([]byte{0x00}))
		exports = append(exports, wasmExport("_start", 0x00, 4))
	}
	module := append([]byte{0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00},
		wasmSection(1, wasmVec(types))...)
	module = append(module, wasmSection(3, wasmVec(funcs))...)
	module = append(module, wasmSection(5, []byte{0x01, 0x00, 0x01})...)
	module = append(module, wasmSection(7, wasmVec(exports))...)
	return append(module, wasmSection(10, wasmVec(bodies))...)
}

func TestNewRejectsInvalidModule(t *testing.T) {
	for _, bad := range [][]byte{nil, []byte("garbage"), {0, 'a', 's', 'm', 1, 0, 0, 0, 0xff}} {
		if _, err := New(context.Background(), bad); !errors.Is(err, ErrInvalidModule) {
			t.Fatalf("expected ErrInvalidModule for %v, got %v", bad[:min(len(bad), 8)], err)
		}
	}
}

func TestNewRejectsWrongABISignature(t *testing.T) {
	spec := defaultModule()
	spec.abiResult = wasmI64
	spec.abiValue = 1
	if _, err := New(context.Background(), buildModule(spec)); err == nil {
		t.Fatal("expected refusal for i64 ABI result")
	}
}

func TestNewRejectsWrongAllocSignature(t *testing.T) {
	spec := defaultModule()
	spec.allocParams = nil
	if _, err := New(context.Background(), buildModule(spec)); err == nil {
		t.Fatal("expected refusal for empty alloc params")
	}
}

func TestNewRejectsWrongMemoryExport(t *testing.T) {
	spec := defaultModule()
	spec.memoryName = "other"
	if _, err := New(context.Background(), buildModule(spec)); err == nil {
		t.Fatal("expected refusal for memory named other")
	}
}

func TestNewIgnoresExportedStartFunction(t *testing.T) {
	spec := defaultModule()
	spec.extraStart = true
	kernel, err := New(context.Background(), buildModule(spec))
	if err != nil {
		t.Fatalf("expected success with exported _start, got %v", err)
	}
	defer kernel.Close(context.Background())
}

func TestInvokeRejectsZeroOutputPointer(t *testing.T) {
	spec := defaultModule()
	spec.callValue = 1 << 32
	kernel, err := New(context.Background(), buildModule(spec))
	if err != nil {
		t.Fatalf("init: %v", err)
	}
	defer kernel.Close(context.Background())
	if _, err := kernel.Invoke(context.Background(), []byte("{}")); !errors.Is(err, ErrBuffer) {
		t.Fatalf("expected ErrBuffer for zero pointer with length, got %v", err)
	}
}

type cancellingMemory struct {
	api.Memory
	cancel context.CancelFunc
}

func (m cancellingMemory) Read(offset, count uint32) ([]byte, bool) {
	data, ok := m.Memory.Read(offset, count)
	if m.cancel != nil {
		m.cancel()
	}
	return data, ok
}

func TestInvokeSurvivesCancellationAfterOutputRead(t *testing.T) {
	ctx := context.Background()
	runtime := wazero.NewRuntimeWithConfig(ctx, wazero.NewRuntimeConfig().WithCloseOnContextDone(true))
	module, err := runtime.InstantiateWithConfig(ctx, buildModule(defaultModule()), wazero.NewModuleConfig())
	if err != nil {
		t.Fatalf("instantiate: %v", err)
	}
	request, cancel := context.WithCancel(context.Background())
	kernel := &Kernel{
		runtime: runtime,
		module:  module,
		memory:  cancellingMemory{Memory: module.ExportedMemory("memory"), cancel: cancel},
		alloc:   module.ExportedFunction("sb_alloc"),
		call:    module.ExportedFunction("sb_call"),
		release: module.ExportedFunction("sb_release"),
		gate:    make(chan struct{}, 1),
	}
	out, err := kernel.Invoke(request, []byte("{}"))
	if err != nil {
		t.Fatalf("expected response despite post-read cancellation, got %v", err)
	}
	if string(out) != "{}" {
		t.Fatalf("unexpected response %q", out)
	}
	_ = kernel.Close(ctx)
}

func TestInvokeRespectsCancelledContextAcquisition(t *testing.T) {
	kernel := &Kernel{gate: make(chan struct{}, 1)}
	kernel.gate <- struct{}{}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := kernel.Invoke(ctx, []byte("{}")); !errors.Is(err, context.Canceled) {
		t.Fatalf("expected context.Canceled, got %v", err)
	}
}

func TestParseResponseEnvelopes(t *testing.T) {
	envelope := func(body string) []byte { return []byte(`{"protocol":"` + protocol + `",` + body + `}`) }
	result, err := parseResponse(envelope(`"result":{"a":1}`))
	if err != nil || string(result) != `{"a":1}` {
		t.Fatalf("valid result: %s %v", result, err)
	}
	result, err = parseResponse(envelope(`"result":null`))
	if err != nil || string(result) != "null" {
		t.Fatalf("result null must succeed: %s %v", result, err)
	}
	if _, err := parseResponse(envelope(`"error":{"code":"invalid_payload"}`)); err == nil {
		t.Fatal("expected refusal")
	} else {
		var refused *RefusedError
		if !errors.As(err, &refused) || refused.Code != "invalid_payload" {
			t.Fatalf("expected RefusedError invalid_payload, got %v", err)
		}
	}
	for _, code := range []string{
		"usage_declaration_invalid",
		"usage_schema_unsupported",
		"usage_record_invalid",
		"usage_detail_session_missing",
		"usage_counter_overflow",
		"usage_reading_incomplete",
		"usage_reading_inconsistent",
		"usage_series_mismatch",
	} {
		_, err := parseResponse(envelope(`"error":{"code":"` + code + `"}`))
		var refused *RefusedError
		if !errors.As(err, &refused) || refused.Code != code {
			t.Fatalf("expected RefusedError %s, got %v", code, err)
		}
	}
	for _, body := range []string{
		`"result":{},"error":{"code":"invalid_request"}`,
		`"result":{},"error":null`,
		`"error":null`,
		`"error":{}`,
		`"error":{"code":""}`,
		`"error":{"code":1}`,
		`"error":{"code":"synthetic-private-sentinel"}`,
		`"protocol_missing":true`,
	} {
		got := envelope(body)
		if body == `"protocol_missing":true` {
			got = []byte(`{"protocol":"` + protocol + `"}`)
		}
		if _, err := parseResponse(got); !errors.Is(err, ErrEnvelope) {
			t.Fatalf("expected ErrEnvelope for %s, got %v", body, err)
		}
	}
	for _, raw := range [][]byte{
		[]byte(`{"protocol":"other","result":{}}`),
		[]byte(`not-json`),
		[]byte(`{}`),
	} {
		if _, err := parseResponse(raw); !errors.Is(err, ErrEnvelope) {
			t.Fatalf("expected ErrEnvelope for %s, got %v", raw, err)
		}
	}
}
