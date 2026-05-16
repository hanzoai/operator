// Package status — convergence helpers.
//
// Mirrors hanzo-operator-core::status::convergence (Rust) so the four
// operators in the family — hanzo (this repo, Go), lux, liquidity, zoo
// (Rust, via ~/work/hanzo/operator-core) — pick the network phase the
// same way. The lesson from the lux operator's
// `InfoCondition::ClosedValidatorSet` pattern: distinguish a real chain
// fault from infra-blind reconciles, and never call a closed validator
// set "Degraded" just because 0 inbound peers is the expected state.
package status

// DegradationInputs are the minimum signals every network controller has
// when deciding whether the network is genuinely degraded. Operators
// with richer status compose this with their own checks before phase
// selection.
type DegradationInputs struct {
	// HealthyValidators is the count of validator pods that responded
	// healthy in the last reconcile.
	HealthyValidators int32

	// TotalValidators is the desired validator count (spec.replicas).
	TotalValidators int32

	// UnreachableValidators is the count of pods we could not contact at
	// all — no IP, no health response. When this equals the missing
	// count, we may be infra-blind rather than seeing a real fault.
	UnreachableValidators int32

	// ClosedValidatorSet is true when the network has no external
	// bootstrap nodes (private LB / GKE internal LB / closed mesh). 0
	// inbound peers is expected by design — never a degradation by
	// itself.
	ClosedValidatorSet bool

	// ChainFaultObserved is true when the controller observed at least
	// one bona-fide chain fault: height skew, chain unhealthy, RPC-level
	// error from a pod that responded. Distinct from "couldn't reach the
	// pod".
	ChainFaultObserved bool
}

// IsGenuinelyDegraded returns true only when there's enough signal to
// call the network genuinely degraded (a real chain fault), as opposed
// to infra-blind (controller can't probe) or closed-set-by-design (0
// inbound peers is expected).
//
// The rule is intentionally conservative: in the absence of a probed
// chain fault, we never escalate to Degraded just because peers are
// missing.
func IsGenuinelyDegraded(in DegradationInputs) bool {
	// Real chain fault overrides everything.
	if in.ChainFaultObserved {
		return true
	}
	// Closed validator set: 0 inbound is by design. Never degraded
	// unless we also saw a real chain fault (handled above).
	if in.ClosedValidatorSet {
		return false
	}
	// Pure infra-blind: every "missing" validator is unreachable. We
	// have no signal that the chain is broken — surface as info-only.
	missing := in.TotalValidators - in.HealthyValidators
	if missing < 0 {
		missing = 0
	}
	if missing > 0 && missing == in.UnreachableValidators {
		return false
	}
	// We could probe pods and they reported back unhealthy: that's a
	// real fault even if we don't have a specific reason yet.
	return missing > 0
}

// PhaseFor picks the canonical phase string for a network reconcile.
// Every operator picks from the same lattice: "Running" | "Degraded".
// Callers map this to their own phase enum — for hanzo-operator,
// "Running" maps to v1alpha1.PhaseRunning and "Degraded" to
// v1alpha1.PhaseDegraded.
func PhaseFor(in DegradationInputs) string {
	if in.HealthyValidators >= in.TotalValidators && !IsGenuinelyDegraded(in) {
		return "Running"
	}
	return "Degraded"
}
