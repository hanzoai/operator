package controller

import (
	"context"
	"fmt"
	"time"

	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"

	"github.com/go-logr/logr"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
	"github.com/hanzoai/operator/internal/metrics"
	"github.com/hanzoai/operator/internal/status"
)

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoplatforms,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoplatforms/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoplatforms/finalizers,verbs=update
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoservices;hanzodatastores;hanzogateways;hanzompcs;hanzonetworks;hanzoingresses,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=networking.k8s.io,resources=networkpolicies,verbs=get;list;watch;create;update;patch;delete

// HanzoPlatformReconciler reconciles a HanzoPlatform object.
type HanzoPlatformReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile handles HanzoPlatform reconciliation.
func (r *HanzoPlatformReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzoplatform", req.NamespacedName)
	start := time.Now()

	defer func() {
		metrics.ReconcileDuration.WithLabelValues("hanzoplatform").Observe(time.Since(start).Seconds())
	}()

	pf := &v1alpha1.HanzoPlatform{}
	if err := r.Get(ctx, req.NamespacedName, pf); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
		return ctrl.Result{}, fmt.Errorf("fetching HanzoPlatform: %w", err)
	}

	if pf.Status.Phase == "" || pf.Status.Phase == v1alpha1.PhasePending {
		pf.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&pf.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	// Reconcile child HanzoService CRs.
	for _, svcSpec := range pf.Spec.Services {
		child := &v1alpha1.HanzoService{
			ObjectMeta: metav1.ObjectMeta{Name: svcSpec.Name, Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, child, func() error {
			if err := ctrl.SetControllerReference(pf, child, r.Scheme); err != nil {
				return err
			}
			child.Spec = svcSpec.Spec
			if child.Spec.Labels == nil {
				child.Spec.Labels = make(map[string]string)
			}
			for k, v := range pf.Spec.Labels {
				child.Spec.Labels[k] = v
			}
			child.Spec.Labels["hanzo.ai/platform"] = pf.Name
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling HanzoService %s: %w", svcSpec.Name, err)
		}
	}

	// Reconcile child HanzoDatastore CRs.
	// HanzoDatastoreSpec has no Labels field; propagate via ObjectMeta.
	for _, dsSpec := range pf.Spec.Datastores {
		child := &v1alpha1.HanzoDatastore{
			ObjectMeta: metav1.ObjectMeta{Name: dsSpec.Name, Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, child, func() error {
			if err := ctrl.SetControllerReference(pf, child, r.Scheme); err != nil {
				return err
			}
			child.Spec = dsSpec.Spec
			childLabels := child.GetLabels()
			if childLabels == nil {
				childLabels = make(map[string]string)
			}
			for k, v := range pf.Spec.Labels {
				childLabels[k] = v
			}
			childLabels["hanzo.ai/platform"] = pf.Name
			child.SetLabels(childLabels)
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling HanzoDatastore %s: %w", dsSpec.Name, err)
		}
	}

	// Reconcile child HanzoGateway CRs.
	// HanzoGatewaySpec has no Labels field; propagate via ObjectMeta.
	for _, gwSpec := range pf.Spec.Gateways {
		child := &v1alpha1.HanzoGateway{
			ObjectMeta: metav1.ObjectMeta{Name: gwSpec.Name, Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, child, func() error {
			if err := ctrl.SetControllerReference(pf, child, r.Scheme); err != nil {
				return err
			}
			child.Spec = gwSpec.Spec
			childLabels := child.GetLabels()
			if childLabels == nil {
				childLabels = make(map[string]string)
			}
			for k, v := range pf.Spec.Labels {
				childLabels[k] = v
			}
			childLabels["hanzo.ai/platform"] = pf.Name
			child.SetLabels(childLabels)
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling HanzoGateway %s: %w", gwSpec.Name, err)
		}
	}

	// Reconcile child HanzoMPC CRs.
	for _, mpcSpec := range pf.Spec.MPCs {
		child := &v1alpha1.HanzoMPC{
			ObjectMeta: metav1.ObjectMeta{Name: mpcSpec.Name, Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, child, func() error {
			if err := ctrl.SetControllerReference(pf, child, r.Scheme); err != nil {
				return err
			}
			child.Spec = mpcSpec.Spec
			if child.Spec.Labels == nil {
				child.Spec.Labels = make(map[string]string)
			}
			for k, v := range pf.Spec.Labels {
				child.Spec.Labels[k] = v
			}
			child.Spec.Labels["hanzo.ai/platform"] = pf.Name
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling HanzoMPC %s: %w", mpcSpec.Name, err)
		}
	}

	// Reconcile child HanzoNetwork CRs.
	for _, netSpec := range pf.Spec.Networks {
		child := &v1alpha1.HanzoNetwork{
			ObjectMeta: metav1.ObjectMeta{Name: netSpec.Name, Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, child, func() error {
			if err := ctrl.SetControllerReference(pf, child, r.Scheme); err != nil {
				return err
			}
			child.Spec = netSpec.Spec
			if child.Spec.Labels == nil {
				child.Spec.Labels = make(map[string]string)
			}
			for k, v := range pf.Spec.Labels {
				child.Spec.Labels[k] = v
			}
			child.Spec.Labels["hanzo.ai/platform"] = pf.Name
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling HanzoNetwork %s: %w", netSpec.Name, err)
		}
	}

	// Reconcile child HanzoIngress CRs.
	for _, ingSpec := range pf.Spec.Ingresses {
		child := &v1alpha1.HanzoIngress{
			ObjectMeta: metav1.ObjectMeta{Name: ingSpec.Name, Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, child, func() error {
			if err := ctrl.SetControllerReference(pf, child, r.Scheme); err != nil {
				return err
			}
			child.Spec = ingSpec.Spec
			if child.Spec.Labels == nil {
				child.Spec.Labels = make(map[string]string)
			}
			for k, v := range pf.Spec.Labels {
				child.Spec.Labels[k] = v
			}
			child.Spec.Labels["hanzo.ai/platform"] = pf.Name
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling HanzoIngress %s: %w", ingSpec.Name, err)
		}
	}

	// Default-deny NetworkPolicy.
	if pf.Spec.NetworkPolicies != nil && pf.Spec.NetworkPolicies.DefaultDeny {
		np := &networkingv1.NetworkPolicy{
			ObjectMeta: metav1.ObjectMeta{Name: pf.Name + "-default-deny", Namespace: pf.Namespace},
		}
		if _, err := ctrl.CreateOrUpdate(ctx, r.Client, np, func() error {
			if err := ctrl.SetControllerReference(pf, np, r.Scheme); err != nil {
				return err
			}
			np.Labels = map[string]string{
				"app.kubernetes.io/managed-by": "hanzo-operator",
				"hanzo.ai/platform":            pf.Name,
			}
			np.Spec = networkingv1.NetworkPolicySpec{
				PodSelector: metav1.LabelSelector{},
				PolicyTypes: []networkingv1.PolicyType{
					networkingv1.PolicyTypeIngress,
					networkingv1.PolicyTypeEgress,
				},
			}
			return nil
		}); err != nil {
			metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
			return ctrl.Result{}, fmt.Errorf("reconciling default-deny NetworkPolicy: %w", err)
		}
	}

	// Aggregate status from child CRs.
	if err := r.Get(ctx, types.NamespacedName{Name: pf.Name, Namespace: pf.Namespace}, pf); err != nil {
		return ctrl.Result{}, fmt.Errorf("re-fetching HanzoPlatform: %w", err)
	}

	serviceCount := int32(len(pf.Spec.Services))
	datastoreCount := int32(len(pf.Spec.Datastores))
	var readyServices int32

	for _, svcSpec := range pf.Spec.Services {
		childSvc := &v1alpha1.HanzoService{}
		if err := r.Get(ctx, types.NamespacedName{Name: svcSpec.Name, Namespace: pf.Namespace}, childSvc); err == nil {
			if childSvc.Status.Phase == v1alpha1.PhaseRunning {
				readyServices++
			}
		}
	}

	for _, gwSpec := range pf.Spec.Gateways {
		serviceCount++
		childGW := &v1alpha1.HanzoGateway{}
		if err := r.Get(ctx, types.NamespacedName{Name: gwSpec.Name, Namespace: pf.Namespace}, childGW); err == nil {
			if childGW.Status.Phase == v1alpha1.PhaseRunning {
				readyServices++
			}
		}
	}
	for _, mpcSpec := range pf.Spec.MPCs {
		serviceCount++
		childMPC := &v1alpha1.HanzoMPC{}
		if err := r.Get(ctx, types.NamespacedName{Name: mpcSpec.Name, Namespace: pf.Namespace}, childMPC); err == nil {
			if childMPC.Status.Phase == v1alpha1.PhaseRunning {
				readyServices++
			}
		}
	}
	for _, netSpec := range pf.Spec.Networks {
		serviceCount++
		childNet := &v1alpha1.HanzoNetwork{}
		if err := r.Get(ctx, types.NamespacedName{Name: netSpec.Name, Namespace: pf.Namespace}, childNet); err == nil {
			if childNet.Status.Phase == v1alpha1.PhaseRunning {
				readyServices++
			}
		}
	}
	for _, ingSpec := range pf.Spec.Ingresses {
		serviceCount++
		childIng := &v1alpha1.HanzoIngress{}
		if err := r.Get(ctx, types.NamespacedName{Name: ingSpec.Name, Namespace: pf.Namespace}, childIng); err == nil {
			if childIng.Status.Phase == v1alpha1.PhaseRunning {
				readyServices++
			}
		}
	}

	pf.Status.ServiceCount = serviceCount
	pf.Status.DatastoreCount = datastoreCount
	pf.Status.ReadyServices = readyServices
	pf.Status.ObservedGeneration = pf.Generation

	if readyServices >= serviceCount && serviceCount > 0 {
		pf.Status.Phase = v1alpha1.PhaseRunning
		status.SetCondition(&pf.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available",
			fmt.Sprintf("%d/%d services ready, %d datastores", readyServices, serviceCount, datastoreCount))
	} else if readyServices > 0 {
		pf.Status.Phase = v1alpha1.PhaseDegraded
		status.SetCondition(&pf.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d services ready", readyServices, serviceCount))
	} else {
		pf.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&pf.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No child services are ready yet")
	}

	if err := r.Status().Update(ctx, pf); err != nil {
		metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "error").Inc()
		return ctrl.Result{}, fmt.Errorf("updating HanzoPlatform status: %w", err)
	}

	metrics.ReconcileTotal.WithLabelValues("hanzoplatform", "success").Inc()
	metrics.ManagedResources.WithLabelValues("hanzoplatform", "HanzoService").Set(float64(len(pf.Spec.Services)))
	metrics.ManagedResources.WithLabelValues("hanzoplatform", "HanzoDatastore").Set(float64(len(pf.Spec.Datastores)))
	log.Info("Reconciliation complete", "phase", pf.Status.Phase,
		"serviceCount", serviceCount, "readyServices", readyServices)
	return ctrl.Result{}, nil
}

// SetupWithManager registers the HanzoPlatform controller with the manager.
func (r *HanzoPlatformReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoPlatform{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&v1alpha1.HanzoService{}).
		Owns(&v1alpha1.HanzoDatastore{}).
		Owns(&v1alpha1.HanzoGateway{}).
		Owns(&v1alpha1.HanzoMPC{}).
		Owns(&v1alpha1.HanzoNetwork{}).
		Owns(&v1alpha1.HanzoIngress{}).
		Named("hanzoplatform").
		Complete(r)
}
