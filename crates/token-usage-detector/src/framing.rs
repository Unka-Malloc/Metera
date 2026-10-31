//! A negotiated frame bound and the control reservation that goes with it.
//!
//! A proposal below the protocol floor is refused outright; the negotiated
//! result is the smaller of the two valid proposals, capped at the accepted
//! maximum, so neither side can raise the other's memory ceiling by asking.

use crate::failure::UsageFailure;

/// The smallest frame bound either side may publish.
pub const MIN_MAX_FRAME_BYTES: usize = 4 * 1024;
/// The largest frame bound this negotiation accepts.
pub const MAX_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// The frame bound a side uses when it never declared one.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Control frames reserved ahead of queued data on a single-stream carrier.
pub const RESERVED_CONTROL_SLOTS: usize = 4;

/// A negotiated frame bound and the control reservation that goes with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Framing {
    host_max_frame_bytes: usize,
    extension_max_frame_bytes: usize,
    control_slots: usize,
}

impl Framing {
    /// Propose a bound. A proposal below [`MIN_MAX_FRAME_BYTES`] violates the
    /// protocol floor and is refused: honoring it silently would either let
    /// this side send frames past the peer's declared ceiling or accept a
    /// bound no valid frame can fill. The negotiated result is the smaller
    /// of the two valid proposals, capped at [`MAX_MAX_FRAME_BYTES`], so
    /// neither side can raise the other's memory ceiling by asking.
    pub fn new(
        host_max_frame_bytes: usize,
        extension_max_frame_bytes: usize,
    ) -> Result<Self, UsageFailure> {
        for (field, proposal) in [
            ("hostMaxFrameBytes", host_max_frame_bytes),
            ("extensionMaxFrameBytes", extension_max_frame_bytes),
        ] {
            if proposal < MIN_MAX_FRAME_BYTES {
                return Err(crate::failure::refusal(
                    "transport_frame_floor_violation",
                    "usage-source/transport",
                )
                .with_field(field)
                .with_presentation_arg("minimum", &MIN_MAX_FRAME_BYTES.to_string()));
            }
        }
        Ok(Self {
            host_max_frame_bytes,
            extension_max_frame_bytes,
            control_slots: RESERVED_CONTROL_SLOTS,
        })
    }

    /// The smaller of the two proposals, capped at the accepted maximum.
    /// Both proposals were validated at construction, so the floor can
    /// never be undercut.
    pub const fn negotiated_max_frame_bytes(self) -> usize {
        let smaller = if self.host_max_frame_bytes < self.extension_max_frame_bytes {
            self.host_max_frame_bytes
        } else {
            self.extension_max_frame_bytes
        };
        if smaller > MAX_MAX_FRAME_BYTES {
            MAX_MAX_FRAME_BYTES
        } else {
            smaller
        }
    }

    pub const fn control_slots(self) -> usize {
        self.control_slots
    }

    /// The bytes of queued data that may precede a control frame on a
    /// single-stream carrier: the honest bound to publish next to a
    /// cancellation.
    pub const fn worst_case_control_wait_bytes(self) -> usize {
        self.negotiated_max_frame_bytes() * self.control_slots
    }

    /// Refuse a frame that exceeds what was negotiated. The refusal names the
    /// bound so the sender can chunk or hand over a blob handle instead.
    pub fn check_frame(&self, frame_bytes: usize) -> Result<(), UsageFailure> {
        let bound = self.negotiated_max_frame_bytes();
        if frame_bytes <= bound {
            return Ok(());
        }
        Err(
            crate::failure::refusal("transport_frame_oversize", "usage-source/transport")
                .with_field("maxFrameBytes")
                .with_presentation_arg("limit", &bound.to_string()),
        )
    }
}

impl Default for Framing {
    fn default() -> Self {
        Self {
            host_max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            extension_max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            control_slots: RESERVED_CONTROL_SLOTS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proposal_below_the_protocol_floor_is_refused() {
        for (host, extension) in [
            (MIN_MAX_FRAME_BYTES - 1, DEFAULT_MAX_FRAME_BYTES),
            (DEFAULT_MAX_FRAME_BYTES, MIN_MAX_FRAME_BYTES - 1),
            (0, 0),
        ] {
            assert!(
                Framing::new(host, extension).is_err(),
                "a subminimum proposal is refused: {host}/{extension}"
            );
        }
    }

    #[test]
    fn the_negotiated_bound_never_exceeds_either_proposal() {
        for (host, extension) in [
            (MIN_MAX_FRAME_BYTES, 64 * 1024),
            (64 * 1024, MIN_MAX_FRAME_BYTES),
            (16 * 1024 * 1024, 8 * 1024),
            (MAX_MAX_FRAME_BYTES * 2, MAX_MAX_FRAME_BYTES),
        ] {
            let negotiated = Framing::new(host, extension)
                .unwrap()
                .negotiated_max_frame_bytes();
            assert!(
                negotiated <= host.min(extension),
                "the negotiated bound respects both ceilings: {negotiated} <= min({host}, {extension})"
            );
        }
    }
}
