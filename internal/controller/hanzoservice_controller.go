package controller

import (
	"context"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	autoscalingv2 "k8s.io/api/autoscaling/v2"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/predicate"

	"github.com/go-logr/logr"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
	"github.com/hanzoai/operator/internal/manifests"
	"github.com/hanzoai/operator/internal/status"
)

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoservices,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoservices/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoservices/finalizers,verbs=update
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=networking.k8s.io,resources=ingresses,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=autoscaling,resources=horizontalpodautoscalers,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=policy,resources=poddisruptionbudgets,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=networking.k8s.io,resources=networkpolicies,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=kms.hanzo.ai,resources=kmssecrets,verbs=get;list;watch;create;update;patch;delete

// HanzoServiceReconciler reconciles a HanzoService object.
type HanzoServiceReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoService.
func (r *HanzoServiceReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzoservice", req.NamespacedName)

	// 1. Fetch the HanzoService CR.
	svc := &v1alpha1.HanzoService{}
	if err := r.Get(ctx, req.NamespacedName, svc); err != nil {
		if errors.IsNotFound(err) {
			log.Info("HanzoService not found, skipping")
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoService: %w", err)
	}

	// 2. Set phase to Creating if this is a new resource.
	if svc.Status.Phase == "" || svc.Status.Phase == v1alpha1.PhasePending {
		status.SetServicePhase(&svc.Status, v1alpha1.PhaseCreating)
		status.SetCondition(&svc.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := svc.Name
	ns := svc.Namespace

	// Labels.
	stdLabels := manifests.StandardLabels(name, svc.Spec.Component, svc.Spec.PartOf, svc.Spec.Image.Tag)
	selectorLabels := manifests.SelectorLabels(name)
	allLabels := manifests.MergeLabels(stdLabels, svc.Spec.Labels)

	// 3. Build and reconcile Deployment.
	containers := r.buildContainers(svc)
	deploy := manifests.BuildDeployment(
		name, ns,
		allLabels, selectorLabels,
		svc.Spec.Replicas,
		containers,
		svc.Spec.Volumes,
		svc.Spec.Strategy,
		svc.Spec.ImagePullSecrets,
		svc.Spec.ServiceAccountName,
	)
	deploy.Spec.Template.Annotations = svc.Spec.Annotations
	deploy.Spec.Template.Spec.InitContainers = svc.Spec.InitContainers
	if err := r.createOrUpdate(ctx, svc, deploy); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Deployment: %w", err)
	}

	// 4. Build and reconcile Service (only if ports are defined).
	if len(svc.Spec.Ports) > 0 {
		svcPorts := r.buildServicePorts(svc.Spec.Ports)
		k8sSvc := manifests.BuildService(name, ns, allLabels, svcPorts, selectorLabels)
		if err := r.createOrUpdate(ctx, svc, k8sSvc); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling Service: %w", err)
		}
	}

	// 5. Ingress.
	if svc.Spec.Ingress != nil && svc.Spec.Ingress.Enabled && len(svc.Spec.Ports) > 0 {
		primaryPort := r.primaryServicePort(svc.Spec.Ports)
		ing := manifests.BuildIngress(name, ns, svc.Spec.Ingress, name, primaryPort, allLabels)
		if err := r.createOrUpdate(ctx, svc, ing); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling Ingress: %w", err)
		}
	}

	// 6. HPA.
	if svc.Spec.Autoscaling != nil && svc.Spec.Autoscaling.Enabled {
		targetRef := autoscalingv2.CrossVersionObjectReference{
			APIVersion: "apps/v1",
			Kind:       "Deployment",
			Name:       name,
		}
		hpa := manifests.BuildHPA(name, ns, targetRef, svc.Spec.Autoscaling, allLabels)
		if err := r.createOrUpdate(ctx, svc, hpa); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling HPA: %w", err)
		}
	}

	// 7. PDB.
	if svc.Spec.PDB != nil && svc.Spec.PDB.Enabled {
		pdb := manifests.BuildPDB(name, ns, svc.Spec.PDB, selectorLabels, allLabels)
		if err := r.createOrUpdate(ctx, svc, pdb); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling PDB: %w", err)
		}
	}

	// 8. NetworkPolicy.
	if svc.Spec.NetworkPolicy != nil && (svc.Spec.NetworkPolicy.Enabled == nil || *svc.Spec.NetworkPolicy.Enabled) {
		np := manifests.BuildNetworkPolicy(name, ns, svc.Spec.NetworkPolicy, selectorLabels, allLabels)
		if err := r.createOrUpdate(ctx, svc, np); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling NetworkPolicy: %w", err)
		}
	}

	// 9. KMSSecret CRs.
	for i := range svc.Spec.KMSSecrets {
		kmsRef := &svc.Spec.KMSSecrets[i]
		if err := r.reconcileKMSSecret(ctx, svc, kmsRef, allLabels); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling KMSSecret %s: %w", kmsRef.ManagedSecretName, err)
		}
	}

	// 10. Check deployment readiness and update status.
	currentDeploy := &appsv1.Deployment{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(deploy), currentDeploy); err != nil {
		return ctrl.Result{}, fmt.Errorf("fetching Deployment status: %w", err)
	}

	svc.Status.ReadyReplicas = currentDeploy.Status.ReadyReplicas
	svc.Status.AvailableReplicas = currentDeploy.Status.AvailableReplicas
	svc.Status.ObservedGeneration = svc.Generation

	desiredReplicas := int32(1)
	if svc.Spec.Replicas != nil {
		desiredReplicas = *svc.Spec.Replicas
	}

	if currentDeploy.Status.ReadyReplicas >= desiredReplicas && desiredReplicas > 0 {
		status.SetServicePhase(&svc.Status, v1alpha1.PhaseRunning)
		status.SetCondition(&svc.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available", "All replicas are ready")
		status.SetCondition(&svc.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionFalse, "Complete", "Reconciliation complete")
	} else if currentDeploy.Status.ReadyReplicas > 0 {
		status.SetServicePhase(&svc.Status, v1alpha1.PhaseDegraded)
		status.SetCondition(&svc.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d replicas ready", currentDeploy.Status.ReadyReplicas, desiredReplicas))
		status.SetCondition(&svc.Status.Conditions, v1alpha1.ConditionTypeDegraded,
			metav1.ConditionTrue, "InsufficientReplicas", "Not all replicas are ready")
	} else {
		status.SetServicePhase(&svc.Status, v1alpha1.PhaseCreating)
		status.SetCondition(&svc.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No replicas are ready yet")
	}

	// Build endpoints for status.
	if svc.Spec.Ingress != nil && svc.Spec.Ingress.Enabled {
		svc.Status.Endpoints = make([]string, 0, len(svc.Spec.Ingress.Hosts))
		scheme := "http"
		if svc.Spec.Ingress.TLS {
			scheme = "https"
		}
		for _, h := range svc.Spec.Ingress.Hosts {
			svc.Status.Endpoints = append(svc.Status.Endpoints, fmt.Sprintf("%s://%s", scheme, h))
		}
	}

	// 11. Update status.
	if err := r.Status().Update(ctx, svc); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoService status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", svc.Status.Phase)
	return ctrl.Result{}, nil
}

// buildContainers constructs the main container plus any sidecars.
func (r *HanzoServiceReconciler) buildContainers(svc *v1alpha1.HanzoService) []corev1.Container {
	image := svc.Spec.Image.Repository
	if svc.Spec.Image.Tag != "" {
		image = image + ":" + svc.Spec.Image.Tag
	}

	main := corev1.Container{
		Name:            svc.Name,
		Image:           image,
		ImagePullPolicy: svc.Spec.Image.PullPolicy,
		Command:         svc.Spec.Command,
		Args:            svc.Spec.Args,
		Env:             svc.Spec.Env,
		EnvFrom:         svc.Spec.EnvFrom,
		VolumeMounts:    svc.Spec.VolumeMounts,
	}

	// Container ports.
	for _, p := range svc.Spec.Ports {
		main.Ports = append(main.Ports, corev1.ContainerPort{
			Name:          p.Name,
			ContainerPort: p.ContainerPort,
			Protocol:      p.Protocol,
		})
	}

	// Resources.
	if svc.Spec.Resources != nil {
		main.Resources = corev1.ResourceRequirements{
			Requests: svc.Spec.Resources.Requests,
			Limits:   svc.Spec.Resources.Limits,
		}
	}

	// Probes.
	if svc.Spec.LivenessProbe != nil {
		main.LivenessProbe = buildHTTPProbe(svc.Spec.LivenessProbe)
	}
	if svc.Spec.ReadinessProbe != nil {
		main.ReadinessProbe = buildHTTPProbe(svc.Spec.ReadinessProbe)
	}

	containers := []corev1.Container{main}
	containers = append(containers, svc.Spec.Sidecars...)
	return containers
}

// buildHTTPProbe converts a ProbeSpec to a Kubernetes probe.
func buildHTTPProbe(spec *v1alpha1.ProbeSpec) *corev1.Probe {
	return &corev1.Probe{
		ProbeHandler: corev1.ProbeHandler{
			HTTPGet: &corev1.HTTPGetAction{
				Path: spec.Path,
				Port: intstr.FromInt32(spec.Port),
			},
		},
		InitialDelaySeconds: spec.InitialDelaySeconds,
		PeriodSeconds:       spec.PeriodSeconds,
	}
}

// buildServicePorts converts v1alpha1 ServicePorts to corev1 ServicePorts.
func (r *HanzoServiceReconciler) buildServicePorts(ports []v1alpha1.ServicePort) []corev1.ServicePort {
	out := make([]corev1.ServicePort, 0, len(ports))
	for _, p := range ports {
		sp := corev1.ServicePort{
			Name:       p.Name,
			Port:       p.ContainerPort,
			TargetPort: intstr.FromInt32(p.ContainerPort),
			Protocol:   p.Protocol,
		}
		if p.ServicePort != nil {
			sp.Port = *p.ServicePort
		}
		out = append(out, sp)
	}
	return out
}

// primaryServicePort returns the port number for the first defined port.
func (r *HanzoServiceReconciler) primaryServicePort(ports []v1alpha1.ServicePort) int32 {
	if len(ports) == 0 {
		return 80
	}
	p := ports[0]
	if p.ServicePort != nil {
		return *p.ServicePort
	}
	return p.ContainerPort
}

// createOrUpdate creates or updates a resource with owner reference and
// mutation function.
func (r *HanzoServiceReconciler) createOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoService,
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

// reconcileKMSSecret creates or updates an unstructured KMSSecret CR.
func (r *HanzoServiceReconciler) reconcileKMSSecret(
	ctx context.Context,
	owner *v1alpha1.HanzoService,
	ref *v1alpha1.KMSSecretRef,
	labels map[string]string,
) error {
	kms := buildKMSSecretUnstructured(owner.Namespace, ref, labels)
	if err := ctrl.SetControllerReference(owner, kms, r.Scheme); err != nil {
		return fmt.Errorf("setting owner reference on KMSSecret: %w", err)
	}

	existing := &unstructured.Unstructured{}
	existing.SetGroupVersionKind(kmsSecretGVK())
	existing.SetName(kms.GetName())
	existing.SetNamespace(kms.GetNamespace())

	result, err := ctrl.CreateOrUpdate(ctx, r.Client, existing, func() error {
		// Copy spec fields from desired.
		desiredSpec, _, _ := unstructured.NestedMap(kms.Object, "spec")
		if desiredSpec != nil {
			if err := unstructured.SetNestedMap(existing.Object, desiredSpec, "spec"); err != nil {
				return err
			}
		}
		// Merge labels.
		existingLabels := existing.GetLabels()
		if existingLabels == nil {
			existingLabels = make(map[string]string)
		}
		for k, v := range kms.GetLabels() {
			existingLabels[k] = v
		}
		existing.SetLabels(existingLabels)
		return nil
	})
	if err != nil {
		return err
	}

	if result != controllerutil.OperationResultNone {
		r.Log.Info("KMSSecret reconciled", "name", kms.GetName(), "result", result)
	}
	return nil
}

func kmsSecretGVK() schema.GroupVersionKind {
	return schema.GroupVersionKind{
		Group:   "kms.hanzo.ai",
		Version: "v1alpha1",
		Kind:    "KMSSecret",
	}
}

func buildKMSSecretUnstructured(namespace string, ref *v1alpha1.KMSSecretRef, labels map[string]string) *unstructured.Unstructured {
	kms := &unstructured.Unstructured{}
	kms.SetGroupVersionKind(kmsSecretGVK())
	kms.SetName(ref.ManagedSecretName)
	kms.SetNamespace(namespace)
	kms.SetLabels(labels)

	spec := map[string]interface{}{
		"hostAPI":     ref.HostAPI,
		"projectSlug": ref.ProjectSlug,
		"envSlug":     ref.EnvSlug,
		"secretsPath": ref.SecretsPath,
		"credentialsRef": map[string]interface{}{
			"name":      ref.CredentialsRef.Name,
			"namespace": ref.CredentialsRef.Namespace,
		},
		"resyncInterval":    int64(ref.ResyncInterval),
		"managedSecretName": ref.ManagedSecretName,
	}
	_ = unstructured.SetNestedMap(kms.Object, spec, "spec")
	return kms
}

// SetupWithManager registers the HanzoService controller with the manager.
func (r *HanzoServiceReconciler) SetupWithManager(mgr ctrl.Manager) error {
	// Build a REST mapper for the KMSSecret watch.
	var restMapper meta.RESTMapper
	if mgr.GetRESTMapper() != nil {
		restMapper = mgr.GetRESTMapper()
	}

	b := ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoService{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.Deployment{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&networkingv1.Ingress{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&autoscalingv2.HorizontalPodAutoscaler{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&policyv1.PodDisruptionBudget{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&networkingv1.NetworkPolicy{}, builder.WithPredicates(updateOrDeletePred()))

	// Watch KMSSecret CRs owned by this controller (only if CRD is installed).
	if restMapper != nil {
		gvk := kmsSecretGVK()
		_, err := restMapper.RESTMapping(schema.GroupKind{Group: gvk.Group, Kind: gvk.Kind}, gvk.Version)
		if err == nil {
			kmsObj := &unstructured.Unstructured{}
			kmsObj.SetGroupVersionKind(gvk)
			b = b.Watches(kmsObj, handler.EnqueueRequestForOwner(
				mgr.GetScheme(), restMapper, &v1alpha1.HanzoService{}, handler.OnlyControllerOwner(),
			))
		} else {
			r.Log.Info("KMSSecret CRD not installed, skipping watch (kms.hanzo.ai/v1alpha1)")
		}
	}

	return b.Named("hanzoservice").Complete(r)
}
