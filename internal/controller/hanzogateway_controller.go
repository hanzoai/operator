package controller

import (
	"context"
	"encoding/json"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/predicate"

	"github.com/go-logr/logr"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
	"github.com/hanzoai/operator/internal/manifests"
	"github.com/hanzoai/operator/internal/status"
)

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzogateways,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzogateways/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzogateways/finalizers,verbs=update
// +kubebuilder:rbac:groups="",resources=configmaps,verbs=get;list;watch;create;update;patch;delete

// HanzoGatewayReconciler reconciles a HanzoGateway object.
type HanzoGatewayReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoGateway.
func (r *HanzoGatewayReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzogateway", req.NamespacedName)

	gw := &v1alpha1.HanzoGateway{}
	if err := r.Get(ctx, req.NamespacedName, gw); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoGateway: %w", err)
	}

	if gw.Status.Phase == "" || gw.Status.Phase == v1alpha1.PhasePending {
		gw.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&gw.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := gw.Name
	ns := gw.Namespace

	stdLabels := manifests.StandardLabels(name, "gateway", "", "")
	selectorLabels := manifests.SelectorLabels(name)

	// Default image.
	image := "ghcr.io/hanzoai/gateway:latest"
	if gw.Spec.Image != nil {
		image = gw.Spec.Image.Repository
		if gw.Spec.Image.Tag != "" {
			image += ":" + gw.Spec.Image.Tag
		}
	}

	// Build gateway config as a ConfigMap.
	configData := r.buildGatewayConfig(gw)
	cm := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name + "-config",
			Namespace: ns,
			Labels:    stdLabels,
		},
		Data: map[string]string{
			"krakend.json": configData,
		},
	}
	if err := r.gatewayCreateOrUpdate(ctx, gw, cm); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling ConfigMap: %w", err)
	}

	// Build Deployment.
	pullPolicy := corev1.PullIfNotPresent
	if gw.Spec.Image != nil && gw.Spec.Image.PullPolicy != "" {
		pullPolicy = gw.Spec.Image.PullPolicy
	}

	container := corev1.Container{
		Name:            name,
		Image:           image,
		ImagePullPolicy: pullPolicy,
		Ports: []corev1.ContainerPort{
			{Name: "http", ContainerPort: 8080, Protocol: corev1.ProtocolTCP},
		},
		VolumeMounts: []corev1.VolumeMount{
			{Name: "config", MountPath: "/etc/krakend", ReadOnly: true},
		},
	}
	if gw.Spec.Resources != nil {
		container.Resources = corev1.ResourceRequirements{
			Requests: gw.Spec.Resources.Requests,
			Limits:   gw.Spec.Resources.Limits,
		}
	}

	volumes := []corev1.Volume{
		{
			Name: "config",
			VolumeSource: corev1.VolumeSource{
				ConfigMap: &corev1.ConfigMapVolumeSource{
					LocalObjectReference: corev1.LocalObjectReference{Name: name + "-config"},
				},
			},
		},
	}

	deploy := manifests.BuildDeployment(
		name, ns, stdLabels, selectorLabels,
		gw.Spec.Replicas,
		[]corev1.Container{container},
		volumes,
		v1alpha1.DeploymentStrategyRollingUpdate,
		nil, "",
	)
	if err := r.gatewayCreateOrUpdate(ctx, gw, deploy); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Deployment: %w", err)
	}

	// Build Service.
	svcPorts := []corev1.ServicePort{
		{Name: "http", Port: 8080, TargetPort: intstr.FromInt32(8080), Protocol: corev1.ProtocolTCP},
	}
	svc := manifests.BuildService(name, ns, stdLabels, svcPorts, selectorLabels)
	if err := r.gatewayCreateOrUpdate(ctx, gw, svc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Service: %w", err)
	}

	// Ingress.
	if gw.Spec.Ingress != nil && gw.Spec.Ingress.Enabled {
		ing := manifests.BuildIngress(name, ns, gw.Spec.Ingress, name, 8080, stdLabels)
		if err := r.gatewayCreateOrUpdate(ctx, gw, ing); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling Ingress: %w", err)
		}
	}

	// Update status.
	currentDeploy := &appsv1.Deployment{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(deploy), currentDeploy); err != nil {
		return ctrl.Result{}, fmt.Errorf("fetching Deployment status: %w", err)
	}

	gw.Status.ReadyReplicas = currentDeploy.Status.ReadyReplicas
	gw.Status.ObservedGeneration = gw.Generation
	gw.Status.RouteCount = int32(len(gw.Spec.Routes))

	desiredReplicas := int32(2)
	if gw.Spec.Replicas != nil {
		desiredReplicas = *gw.Spec.Replicas
	}

	if currentDeploy.Status.ReadyReplicas >= desiredReplicas && desiredReplicas > 0 {
		gw.Status.Phase = v1alpha1.PhaseRunning
		status.SetCondition(&gw.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available", "All replicas are ready")
	} else if currentDeploy.Status.ReadyReplicas > 0 {
		gw.Status.Phase = v1alpha1.PhaseDegraded
		status.SetCondition(&gw.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d replicas ready", currentDeploy.Status.ReadyReplicas, desiredReplicas))
	} else {
		gw.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&gw.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No replicas are ready yet")
	}

	if err := r.Status().Update(ctx, gw); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoGateway status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", gw.Status.Phase)
	return ctrl.Result{}, nil
}

type krakendEndpoint struct {
	Endpoint string           `json:"endpoint"`
	Method   string           `json:"method,omitempty"`
	Backend  []krakendBackend `json:"backend"`
}
type krakendBackend struct {
	Host       []string `json:"host"`
	URLPattern string   `json:"url_pattern"`
}
type krakendConfig struct {
	Version   int               `json:"version"`
	Endpoints []krakendEndpoint `json:"endpoints"`
}

func (r *HanzoGatewayReconciler) buildGatewayConfig(gw *v1alpha1.HanzoGateway) string {
	cfg := krakendConfig{Version: 3}
	for _, route := range gw.Spec.Routes {
		ep := krakendEndpoint{
			Endpoint: route.Prefix,
			Backend: []krakendBackend{
				{
					Host:       []string{"http://" + route.Backend},
					URLPattern: route.Prefix,
				},
			},
		}
		if len(route.Methods) > 0 {
			ep.Method = route.Methods[0]
		}
		cfg.Endpoints = append(cfg.Endpoints, ep)
	}
	data, _ := json.MarshalIndent(cfg, "", "  ")
	return string(data)
}

func (r *HanzoGatewayReconciler) gatewayCreateOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoGateway,
	desired client.Object,
) error {
	if err := ctrl.SetControllerReference(owner, desired, r.Scheme); err != nil {
		return fmt.Errorf("setting owner reference: %w", err)
	}
	existing := desired.DeepCopyObject().(client.Object)
	mutateFn := manifests.MutateFuncFor(existing, desired)
	result, err := ctrl.CreateOrUpdate(ctx, r.Client, existing, mutateFn)
	if err != nil {
		return err
	}
	if result != controllerutil.OperationResultNone {
		r.Log.Info("Resource reconciled",
			"kind", existing.GetObjectKind().GroupVersionKind().Kind,
			"name", existing.GetName(),
			"result", result)
	}
	return nil
}

// SetupWithManager registers the HanzoGateway controller with the manager.
func (r *HanzoGatewayReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoGateway{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.Deployment{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&corev1.ConfigMap{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&networkingv1.Ingress{}, builder.WithPredicates(updateOrDeletePred())).
		Named("hanzogateway").
		Complete(r)
}
