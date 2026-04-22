package status

import (
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
)

// SetCondition upserts a condition by type. If an existing condition of the
// same type is found and its status or reason differs, the condition is
// updated. Otherwise, a new condition is appended.
func SetCondition(
	conditions *[]metav1.Condition,
	condType string,
	status metav1.ConditionStatus,
	reason, message string,
) {
	now := metav1.NewTime(time.Now())
	for i, c := range *conditions {
		if c.Type == condType {
			if c.Status != status || c.Reason != reason {
				(*conditions)[i].Status = status
				(*conditions)[i].Reason = reason
				(*conditions)[i].Message = message
				(*conditions)[i].LastTransitionTime = now
			}
			return
		}
	}
	*conditions = append(*conditions, metav1.Condition{
		Type:               condType,
		Status:             status,
		Reason:             reason,
		Message:            message,
		LastTransitionTime: now,
	})
}

// IsReady returns true when a "Ready" condition with status True exists.
func IsReady(conditions []metav1.Condition) bool {
	for _, c := range conditions {
		if c.Type == v1alpha1.ConditionTypeReady {
			return c.Status == metav1.ConditionTrue
		}
	}
	return false
}

// SetServicePhase sets the Phase field on a HanzoServiceStatus.
func SetServicePhase(s *v1alpha1.HanzoServiceStatus, phase v1alpha1.Phase) {
	s.Phase = phase
}

// SetDatastorePhase sets the Phase field on a HanzoDatastoreStatus.
func SetDatastorePhase(s *v1alpha1.HanzoDatastoreStatus, phase v1alpha1.Phase) {
	s.Phase = phase
}

// SetBaseAppPhase sets the Phase field on a BaseAppStatus.
func SetBaseAppPhase(s *v1alpha1.BaseAppStatus, phase v1alpha1.Phase) {
	s.Phase = phase
}
