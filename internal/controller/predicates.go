package controller

import (
	"sigs.k8s.io/controller-runtime/pkg/event"
	"sigs.k8s.io/controller-runtime/pkg/predicate"
)

// createOrUpdatePred triggers reconciliation on create events or when the
// spec changes (generation bump) or annotations change. It ignores
// status-only updates and delete events.
func createOrUpdatePred() predicate.Predicate {
	return predicate.Funcs{
		CreateFunc: func(_ event.CreateEvent) bool {
			return true
		},
		UpdateFunc: func(e event.UpdateEvent) bool {
			if e.ObjectOld == nil || e.ObjectNew == nil {
				return false
			}
			// Reconcile on spec change (generation bump).
			if e.ObjectNew.GetGeneration() != e.ObjectOld.GetGeneration() {
				return true
			}
			// Reconcile on annotation change.
			return !mapsEqual(e.ObjectOld.GetAnnotations(), e.ObjectNew.GetAnnotations())
		},
		DeleteFunc: func(_ event.DeleteEvent) bool {
			return false
		},
		GenericFunc: func(_ event.GenericEvent) bool {
			return false
		},
	}
}

// updateOrDeletePred triggers reconciliation on spec changes (generation
// bump) or deletion. It ignores create events so that the initial
// reconcile is driven by the primary resource's create predicate.
func updateOrDeletePred() predicate.Predicate {
	return predicate.Funcs{
		CreateFunc: func(_ event.CreateEvent) bool {
			return false
		},
		UpdateFunc: func(e event.UpdateEvent) bool {
			if e.ObjectOld == nil || e.ObjectNew == nil {
				return false
			}
			return e.ObjectNew.GetGeneration() != e.ObjectOld.GetGeneration()
		},
		DeleteFunc: func(_ event.DeleteEvent) bool {
			return true
		},
		GenericFunc: func(_ event.GenericEvent) bool {
			return false
		},
	}
}

// statusChangePred triggers reconciliation when the status subresource
// changes on owned objects like Deployments and StatefulSets. It detects
// status changes by comparing observed generation or resource version
// when generation is unchanged.
func statusChangePred() predicate.Predicate {
	return predicate.Funcs{
		CreateFunc: func(_ event.CreateEvent) bool {
			return false
		},
		UpdateFunc: func(e event.UpdateEvent) bool {
			if e.ObjectOld == nil || e.ObjectNew == nil {
				return false
			}
			// Generation unchanged means only status/metadata changed.
			if e.ObjectNew.GetGeneration() == e.ObjectOld.GetGeneration() {
				return e.ObjectNew.GetResourceVersion() != e.ObjectOld.GetResourceVersion()
			}
			return false
		},
		DeleteFunc: func(_ event.DeleteEvent) bool {
			return false
		},
		GenericFunc: func(_ event.GenericEvent) bool {
			return false
		},
	}
}

// mapsEqual returns true when two string maps have identical keys and values.
func mapsEqual(a, b map[string]string) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range a {
		if b[k] != v {
			return false
		}
	}
	return true
}
