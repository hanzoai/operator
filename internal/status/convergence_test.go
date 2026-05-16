package status

import "testing"

func base() DegradationInputs {
	return DegradationInputs{
		HealthyValidators:     3,
		TotalValidators:       3,
		UnreachableValidators: 0,
		ClosedValidatorSet:    false,
		ChainFaultObserved:    false,
	}
}

func TestFullHealthRuns(t *testing.T) {
	in := base()
	if IsGenuinelyDegraded(in) {
		t.Fatalf("expected not degraded, got degraded")
	}
	if got := PhaseFor(in); got != "Running" {
		t.Fatalf("expected Running, got %q", got)
	}
}

func TestClosedSetWithZeroInboundIsNotDegraded(t *testing.T) {
	in := base()
	in.ClosedValidatorSet = true
	if IsGenuinelyDegraded(in) {
		t.Fatalf("closed set with no fault should not be degraded")
	}
	if got := PhaseFor(in); got != "Running" {
		t.Fatalf("expected Running, got %q", got)
	}
}

func TestClosedSetWithRealFaultIsDegraded(t *testing.T) {
	in := base()
	in.ClosedValidatorSet = true
	in.ChainFaultObserved = true
	if !IsGenuinelyDegraded(in) {
		t.Fatalf("expected degraded when chain fault observed")
	}
	if got := PhaseFor(in); got != "Degraded" {
		t.Fatalf("expected Degraded, got %q", got)
	}
}

func TestInfraBlindIsNotDegraded(t *testing.T) {
	// Every missing validator is unreachable — controller is partitioned,
	// not the chain.
	in := base()
	in.HealthyValidators = 1
	in.UnreachableValidators = 2
	if IsGenuinelyDegraded(in) {
		t.Fatalf("infra-blind should not be 'genuinely degraded'")
	}
	// PhaseFor still returns Degraded because the chain isn't at full
	// capacity, but the helper distinguishes infra-blind from chain-faulty
	// for downstream callers (alerting, auto-recovery).
	if got := PhaseFor(in); got != "Degraded" {
		t.Fatalf("expected Degraded by validator count, got %q", got)
	}
}

func TestProbedUnhealthyIsDegraded(t *testing.T) {
	// We could reach the pod, it just reported unhealthy: that's a real
	// fault.
	in := base()
	in.HealthyValidators = 1
	in.UnreachableValidators = 0
	if !IsGenuinelyDegraded(in) {
		t.Fatalf("probed-unhealthy should be 'genuinely degraded'")
	}
	if got := PhaseFor(in); got != "Degraded" {
		t.Fatalf("expected Degraded, got %q", got)
	}
}

func TestChainFaultOverridesInfraBlind(t *testing.T) {
	in := base()
	in.HealthyValidators = 1
	in.UnreachableValidators = 2
	in.ChainFaultObserved = true
	if !IsGenuinelyDegraded(in) {
		t.Fatalf("chain fault must override infra-blind")
	}
	if got := PhaseFor(in); got != "Degraded" {
		t.Fatalf("expected Degraded, got %q", got)
	}
}
