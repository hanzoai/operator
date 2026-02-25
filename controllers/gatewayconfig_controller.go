package controllers

import (
	"context"
	"fmt"
	"time"

	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	appsv1 "k8s.io/api/apps/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/log"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	gatewayv1alpha1 "github.com/hanzoai/hanzo-operator/api/v1alpha1"
)

// GatewayConfigReconciler reconciles a GatewayConfig object.
type GatewayConfigReconciler struct {
	client.Client
	Scheme *runtime.Scheme
}

// +kubebuilder:rbac:groups=gateway.hanzo.ai,resources=gatewayconfigs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=gateway.hanzo.ai,resources=gatewayconfigs/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=networking.k8s.io,resources=ingresses,verbs=get;list;watch
// +kubebuilder:rbac:groups="",resources=services,verbs=get;list;watch
// +kubebuilder:rbac:groups="",resources=configmaps,verbs=get;list;watch;create;update;patch
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;list;watch;update;patch

// resolveProvider returns the effective IngressProvider, defaulting to traefik.
func resolveProvider(gwc *gatewayv1alpha1.GatewayConfig) gatewayv1alpha1.IngressProvider {
	if gwc.Spec.IngressProvider == gatewayv1alpha1.IngressProviderCustom {
		return gatewayv1alpha1.IngressProviderCustom
	}
	return gatewayv1alpha1.IngressProviderTraefik
}

func (r *GatewayConfigReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	logger := log.FromContext(ctx)

	// Fetch the GatewayConfig instance.
	var gwc gatewayv1alpha1.GatewayConfig
	if err := r.Get(ctx, req.NamespacedName, &gwc); err != nil {
		if apierrors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, err
	}

	provider := resolveProvider(&gwc)
	logger.Info("reconciling GatewayConfig", "name", gwc.Name, "provider", provider)

	// List Ingress resources, optionally filtered by namespace.
	// Both providers need this: custom for config generation, traefik for status reporting.
	ingresses, err := r.listIngresses(ctx, &gwc)
	if err != nil {
		return ctrl.Result{}, fmt.Errorf("listing ingresses: %w", err)
	}

	// Render config in both modes (used for status reporting and conflict detection).
	result, err := renderConfig(ingresses, gwc.Spec.Defaults, gwc.Spec.IngressSelector)
	if err != nil {
		return ctrl.Result{}, fmt.Errorf("rendering config: %w", err)
	}

	logger.Info("rendered config",
		"provider", provider,
		"routes", len(result.Config.Routes),
		"conflicts", len(result.Conflicts),
		"skipped", len(result.Skipped),
		"hash", result.Hash[:12],
	)

	// Check if state has changed since last reconciliation.
	if result.Hash == gwc.Status.LastObservedHash {
		logger.Info("config unchanged, skipping update")
		return ctrl.Result{}, nil
	}

	// For "custom" provider: generate ConfigMap and trigger deployment reload.
	// For "traefik" provider: Traefik watches Ingress resources natively, skip generation.
	if provider == gatewayv1alpha1.IngressProviderCustom {
		cmKey := gwc.Spec.Output.Key
		if cmKey == "" {
			cmKey = "ingress.json"
		}

		if err := r.ensureConfigMap(ctx, &gwc, cmKey, result); err != nil {
			return ctrl.Result{}, fmt.Errorf("ensuring configmap: %w", err)
		}

		if err := r.triggerReload(ctx, &gwc, result.Hash); err != nil {
			return ctrl.Result{}, fmt.Errorf("triggering reload: %w", err)
		}

		gwc.Status.LastAppliedHash = result.Hash
	}

	// Update status (both providers).
	now := metav1.Now()
	gwc.Status.IngressProvider = provider
	gwc.Status.RouteCount = countRoutes(result)
	gwc.Status.LastObservedHash = result.Hash
	gwc.Status.Conflicts = result.Conflicts
	gwc.Status.SkippedIngresses = result.Skipped
	gwc.Status.LastReconcileTime = &now

	// Set Ready condition.
	readyCondition := metav1.Condition{
		Type:               "Ready",
		Status:             metav1.ConditionTrue,
		LastTransitionTime: now,
	}

	switch provider {
	case gatewayv1alpha1.IngressProviderTraefik:
		readyCondition.Reason = "IngressObserved"
		readyCondition.Message = fmt.Sprintf("Observed %d routes (Traefik handles routing natively)", gwc.Status.RouteCount)
		if len(result.Conflicts) > 0 {
			readyCondition.Reason = "IngressObservedWithConflicts"
			readyCondition.Message = fmt.Sprintf("Observed %d routes, %d conflicts (Traefik mode)",
				gwc.Status.RouteCount, len(result.Conflicts))
		}
	case gatewayv1alpha1.IngressProviderCustom:
		readyCondition.Reason = "ConfigApplied"
		readyCondition.Message = fmt.Sprintf("Applied config with %d routes", gwc.Status.RouteCount)
		if len(result.Conflicts) > 0 {
			readyCondition.Reason = "ConfigAppliedWithConflicts"
			readyCondition.Message = fmt.Sprintf("Applied config with %d routes, %d conflicts",
				gwc.Status.RouteCount, len(result.Conflicts))
		}
	}

	setCondition(&gwc.Status.Conditions, readyCondition)

	if err := r.Status().Update(ctx, &gwc); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating status: %w", err)
	}

	logger.Info("reconciliation complete",
		"provider", provider,
		"routeCount", gwc.Status.RouteCount,
		"hash", result.Hash[:12],
	)

	return ctrl.Result{}, nil
}

// listIngresses returns Ingress resources matching the GatewayConfig's filters.
func (r *GatewayConfigReconciler) listIngresses(
	ctx context.Context,
	gwc *gatewayv1alpha1.GatewayConfig,
) ([]networkingv1.Ingress, error) {
	var allIngresses []networkingv1.Ingress

	namespaces := []string{""}
	if gwc.Spec.Namespaces != nil && len(gwc.Spec.Namespaces.Include) > 0 {
		namespaces = gwc.Spec.Namespaces.Include
	}

	for _, ns := range namespaces {
		var list networkingv1.IngressList
		opts := []client.ListOption{}
		if ns != "" {
			opts = append(opts, client.InNamespace(ns))
		}
		if err := r.List(ctx, &list, opts...); err != nil {
			return nil, fmt.Errorf("listing ingresses in namespace %q: %w", ns, err)
		}
		allIngresses = append(allIngresses, list.Items...)
	}

	return allIngresses, nil
}

// ensureConfigMap creates or updates the target ConfigMap.
func (r *GatewayConfigReconciler) ensureConfigMap(
	ctx context.Context,
	gwc *gatewayv1alpha1.GatewayConfig,
	key string,
	result *renderResult,
) error {
	cmName := types.NamespacedName{
		Namespace: gwc.Namespace,
		Name:      gwc.Spec.Output.ConfigMapName,
	}

	var cm corev1.ConfigMap
	err := r.Get(ctx, cmName, &cm)
	if apierrors.IsNotFound(err) {
		// Create new ConfigMap.
		cm = corev1.ConfigMap{
			ObjectMeta: metav1.ObjectMeta{
				Name:      cmName.Name,
				Namespace: cmName.Namespace,
				Labels: map[string]string{
					"app.kubernetes.io/managed-by": "hanzo-operator",
					"gateway.hanzo.ai/config":      gwc.Name,
				},
			},
			Data: map[string]string{
				key: string(result.JSON),
			},
		}
		// Set owner reference for garbage collection.
		if err := ctrl.SetControllerReference(gwc, &cm, r.Scheme); err != nil {
			return fmt.Errorf("setting owner reference: %w", err)
		}
		return r.Create(ctx, &cm)
	}
	if err != nil {
		return fmt.Errorf("getting configmap: %w", err)
	}

	// Update existing ConfigMap.
	if cm.Data == nil {
		cm.Data = make(map[string]string)
	}
	cm.Data[key] = string(result.JSON)
	if cm.Labels == nil {
		cm.Labels = make(map[string]string)
	}
	cm.Labels["app.kubernetes.io/managed-by"] = "hanzo-operator"
	cm.Labels["gateway.hanzo.ai/config"] = gwc.Name

	return r.Update(ctx, &cm)
}

// triggerReload restarts the target deployment based on the reload strategy.
func (r *GatewayConfigReconciler) triggerReload(
	ctx context.Context,
	gwc *gatewayv1alpha1.GatewayConfig,
	hash string,
) error {
	strategy := gwc.Spec.Target.ReloadStrategy
	if strategy == "" {
		strategy = gatewayv1alpha1.ReloadStrategyRollout
	}

	if strategy == gatewayv1alpha1.ReloadStrategyHotReload {
		// Hot reload: the gateway watches the ConfigMap mount for changes.
		// No deployment restart needed.
		return nil
	}

	// Rollout strategy: patch the deployment annotation to trigger a rolling restart.
	deployName := types.NamespacedName{
		Namespace: gwc.Namespace,
		Name:      gwc.Spec.Target.DeploymentName,
	}

	var deploy appsv1.Deployment
	if err := r.Get(ctx, deployName, &deploy); err != nil {
		if apierrors.IsNotFound(err) {
			log.FromContext(ctx).Info("target deployment not found, skipping reload",
				"deployment", deployName.Name)
			return nil
		}
		return fmt.Errorf("getting deployment: %w", err)
	}

	if deploy.Spec.Template.Annotations == nil {
		deploy.Spec.Template.Annotations = make(map[string]string)
	}
	deploy.Spec.Template.Annotations["gateway.hanzo.ai/config-hash"] = hash
	deploy.Spec.Template.Annotations["gateway.hanzo.ai/restart-at"] = time.Now().UTC().Format(time.RFC3339)

	return r.Update(ctx, &deploy)
}

// countRoutes returns total number of backend entries across all hosts.
func countRoutes(result *renderResult) int {
	count := 0
	for _, hr := range result.Config.Routes {
		count += len(hr.Backends)
	}
	return count
}

// setCondition adds or updates a condition in the conditions slice.
func setCondition(conditions *[]metav1.Condition, condition metav1.Condition) {
	for i, c := range *conditions {
		if c.Type == condition.Type {
			(*conditions)[i] = condition
			return
		}
	}
	*conditions = append(*conditions, condition)
}

// SetupWithManager registers the controller with the manager.
func (r *GatewayConfigReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&gatewayv1alpha1.GatewayConfig{}).
		Owns(&corev1.ConfigMap{}).
		Watches(
			&networkingv1.Ingress{},
			handler.EnqueueRequestsFromMapFunc(r.findGatewayConfigsForIngress),
		).
		Complete(r)
}

// findGatewayConfigsForIngress maps an Ingress change to all GatewayConfig resources
// that should be reconciled.
func (r *GatewayConfigReconciler) findGatewayConfigsForIngress(
	ctx context.Context,
	obj client.Object,
) []reconcile.Request {
	var gwcList gatewayv1alpha1.GatewayConfigList
	if err := r.List(ctx, &gwcList); err != nil {
		log.FromContext(ctx).Error(err, "unable to list GatewayConfigs for Ingress mapping")
		return nil
	}

	var requests []reconcile.Request
	for _, gwc := range gwcList.Items {
		// If namespace filtering is configured, check if the Ingress's namespace is included.
		if gwc.Spec.Namespaces != nil && len(gwc.Spec.Namespaces.Include) > 0 {
			found := false
			for _, ns := range gwc.Spec.Namespaces.Include {
				if ns == obj.GetNamespace() {
					found = true
					break
				}
			}
			if !found {
				continue
			}
		}

		requests = append(requests, reconcile.Request{
			NamespacedName: types.NamespacedName{
				Name:      gwc.Name,
				Namespace: gwc.Namespace,
			},
		})
	}

	return requests
}
