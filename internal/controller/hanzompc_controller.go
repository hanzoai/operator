package controller

import (
	"context"
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

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzompcs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzompcs/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzompcs/finalizers,verbs=update

// HanzoMPCReconciler reconciles a HanzoMPC object.
type HanzoMPCReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoMPC.
func (r *HanzoMPCReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzompc", req.NamespacedName)

	mpc := &v1alpha1.HanzoMPC{}
	if err := r.Get(ctx, req.NamespacedName, mpc); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoMPC: %w", err)
	}

	if mpc.Status.Phase == "" || mpc.Status.Phase == v1alpha1.PhasePending {
		mpc.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&mpc.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := mpc.Name
	ns := mpc.Namespace
	headlessSvcName := name + "-headless"

	stdLabels := manifests.StandardLabels(name, "mpc", "", "")
	selectorLabels := manifests.SelectorLabels(name)
	allLabels := manifests.MergeLabels(stdLabels, mpc.Spec.Labels)

	image := mpc.Spec.Image.Repository
	if mpc.Spec.Image.Tag != "" {
		image += ":" + mpc.Spec.Image.Tag
	}

	p2pPort := mpc.Spec.P2PPort
	if p2pPort == 0 {
		p2pPort = 4000
	}
	apiPort := mpc.Spec.APIPort
	if apiPort == 0 {
		apiPort = 8080
	}

	// Build peer list via DNS.
	peerEnv := ""
	for i := int32(0); i < mpc.Spec.Replicas; i++ {
		if i > 0 {
			peerEnv += ","
		}
		peerEnv += fmt.Sprintf("%s-%d.%s.%s.svc.cluster.local:%d", name, i, headlessSvcName, ns, p2pPort)
	}

	mainContainer := corev1.Container{
		Name:            name,
		Image:           image,
		ImagePullPolicy: mpc.Spec.Image.PullPolicy,
		Ports: []corev1.ContainerPort{
			{Name: "p2p", ContainerPort: p2pPort, Protocol: corev1.ProtocolTCP},
			{Name: "api", ContainerPort: apiPort, Protocol: corev1.ProtocolTCP},
		},
		Env: []corev1.EnvVar{
			{Name: "MPC_PEERS", Value: peerEnv},
			{Name: "MPC_THRESHOLD", Value: fmt.Sprintf("%d", mpc.Spec.Threshold)},
			{Name: "MPC_REPLICAS", Value: fmt.Sprintf("%d", mpc.Spec.Replicas)},
		},
		ReadinessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{
				TCPSocket: &corev1.TCPSocketAction{Port: intstr.FromInt32(apiPort)},
			},
			InitialDelaySeconds: 10,
			PeriodSeconds:       10,
		},
	}
	if mpc.Spec.Resources != nil {
		mainContainer.Resources = corev1.ResourceRequirements{
			Requests: mpc.Spec.Resources.Requests,
			Limits:   mpc.Spec.Resources.Limits,
		}
	}

	var imagePullSecrets []corev1.LocalObjectReference
	for _, s := range mpc.Spec.ImagePullSecrets {
		imagePullSecrets = append(imagePullSecrets, corev1.LocalObjectReference{Name: s})
	}

	replicas := mpc.Spec.Replicas
	sts := manifests.BuildStatefulSet(
		name, ns, allLabels, selectorLabels,
		&replicas,
		[]corev1.Container{mainContainer},
		nil, nil,
		imagePullSecrets,
		headlessSvcName,
	)
	sts.Spec.Template.Annotations = mpc.Spec.Annotations
	if err := r.mpcCreateOrUpdate(ctx, mpc, sts); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling StatefulSet: %w", err)
	}

	// Headless Service.
	svcPorts := []corev1.ServicePort{
		{Name: "p2p", Port: p2pPort, TargetPort: intstr.FromInt32(p2pPort), Protocol: corev1.ProtocolTCP},
		{Name: "api", Port: apiPort, TargetPort: intstr.FromInt32(apiPort), Protocol: corev1.ProtocolTCP},
	}
	headlessSvc := manifests.BuildHeadlessService(headlessSvcName, ns, allLabels, svcPorts, selectorLabels)
	if err := r.mpcCreateOrUpdate(ctx, mpc, headlessSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling headless Service: %w", err)
	}

	// Regular Service.
	regularSvc := manifests.BuildService(name, ns, allLabels, svcPorts, selectorLabels)
	if err := r.mpcCreateOrUpdate(ctx, mpc, regularSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Service: %w", err)
	}

	// Dashboard Deployment.
	if mpc.Spec.Dashboard != nil && mpc.Spec.Dashboard.Enabled {
		dashName := name + "-dashboard"
		dashImage := mpc.Spec.Dashboard.Image
		if dashImage == "" {
			dashImage = "ghcr.io/hanzoai/mpc-dashboard:latest"
		}
		dashContainer := corev1.Container{
			Name:  dashName,
			Image: dashImage,
			Ports: []corev1.ContainerPort{
				{Name: "http", ContainerPort: 3000, Protocol: corev1.ProtocolTCP},
			},
			Env: []corev1.EnvVar{
				{Name: "MPC_API_URL", Value: fmt.Sprintf("http://%s:%d", name, apiPort)},
			},
		}
		if mpc.Spec.Dashboard.Resources != nil {
			dashContainer.Resources = corev1.ResourceRequirements{
				Requests: mpc.Spec.Dashboard.Resources.Requests,
				Limits:   mpc.Spec.Dashboard.Resources.Limits,
			}
		}
		dashLabels := manifests.StandardLabels(dashName, "mpc-dashboard", "", "")
		dashSelLabels := manifests.SelectorLabels(dashName)
		one := int32(1)
		dashDeploy := manifests.BuildDeployment(
			dashName, ns, dashLabels, dashSelLabels,
			&one, []corev1.Container{dashContainer}, nil,
			v1alpha1.DeploymentStrategyRollingUpdate, imagePullSecrets, "",
		)
		if err := r.mpcCreateOrUpdate(ctx, mpc, dashDeploy); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling dashboard Deployment: %w", err)
		}
		dashSvcPorts := []corev1.ServicePort{
			{Name: "http", Port: 3000, TargetPort: intstr.FromInt32(3000), Protocol: corev1.ProtocolTCP},
		}
		dashSvc := manifests.BuildService(dashName, ns, dashLabels, dashSvcPorts, dashSelLabels)
		if err := r.mpcCreateOrUpdate(ctx, mpc, dashSvc); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling dashboard Service: %w", err)
		}
	}

	// Cache Deployment.
	if mpc.Spec.Cache != nil && mpc.Spec.Cache.Enabled {
		cacheName := name + "-cache"
		cacheImage := mpc.Spec.Cache.Image
		if cacheImage == "" {
			cacheImage = "ghcr.io/hanzoai/kv:8"
		}
		cacheContainer := corev1.Container{
			Name:  cacheName,
			Image: cacheImage,
			Ports: []corev1.ContainerPort{
				{Name: "redis", ContainerPort: 6379, Protocol: corev1.ProtocolTCP},
			},
		}
		if mpc.Spec.Cache.Resources != nil {
			cacheContainer.Resources = corev1.ResourceRequirements{
				Requests: mpc.Spec.Cache.Resources.Requests,
				Limits:   mpc.Spec.Cache.Resources.Limits,
			}
		}
		cacheLabels := manifests.StandardLabels(cacheName, "mpc-cache", "", "")
		cacheSelLabels := manifests.SelectorLabels(cacheName)
		one := int32(1)
		cacheDeploy := manifests.BuildDeployment(
			cacheName, ns, cacheLabels, cacheSelLabels,
			&one, []corev1.Container{cacheContainer}, nil,
			v1alpha1.DeploymentStrategyRollingUpdate, imagePullSecrets, "",
		)
		if err := r.mpcCreateOrUpdate(ctx, mpc, cacheDeploy); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling cache Deployment: %w", err)
		}
		cacheSvcPorts := []corev1.ServicePort{
			{Name: "redis", Port: 6379, TargetPort: intstr.FromInt32(6379), Protocol: corev1.ProtocolTCP},
		}
		cacheSvc := manifests.BuildService(cacheName, ns, cacheLabels, cacheSvcPorts, cacheSelLabels)
		if err := r.mpcCreateOrUpdate(ctx, mpc, cacheSvc); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling cache Service: %w", err)
		}
	}

	// Ingress.
	if mpc.Spec.Ingress != nil && mpc.Spec.Ingress.Enabled {
		ing := manifests.BuildIngress(name, ns, mpc.Spec.Ingress, name, apiPort, allLabels)
		if err := r.mpcCreateOrUpdate(ctx, mpc, ing); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling Ingress: %w", err)
		}
	}

	// Update status.
	currentSTS := &appsv1.StatefulSet{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(sts), currentSTS); err != nil {
		return ctrl.Result{}, fmt.Errorf("fetching StatefulSet status: %w", err)
	}

	mpc.Status.ReadyNodes = currentSTS.Status.ReadyReplicas
	mpc.Status.ObservedGeneration = mpc.Generation

	if currentSTS.Status.ReadyReplicas >= mpc.Spec.Replicas {
		mpc.Status.Phase = v1alpha1.PhaseRunning
		status.SetCondition(&mpc.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available", "All MPC nodes are ready")
	} else if currentSTS.Status.ReadyReplicas > 0 {
		mpc.Status.Phase = v1alpha1.PhaseDegraded
		status.SetCondition(&mpc.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d nodes ready", currentSTS.Status.ReadyReplicas, mpc.Spec.Replicas))
	} else {
		mpc.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&mpc.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No MPC nodes are ready yet")
	}

	if err := r.Status().Update(ctx, mpc); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoMPC status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", mpc.Status.Phase)
	return ctrl.Result{}, nil
}

func (r *HanzoMPCReconciler) mpcCreateOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoMPC,
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

// SetupWithManager registers the HanzoMPC controller with the manager.
func (r *HanzoMPCReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoMPC{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.StatefulSet{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&appsv1.Deployment{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&networkingv1.Ingress{}, builder.WithPredicates(updateOrDeletePred())).
		Named("hanzompc").
		Complete(r)
}
