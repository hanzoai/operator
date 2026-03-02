package controller

import (
	"context"
	"fmt"
	"strings"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
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

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzonetworks,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzonetworks/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzonetworks/finalizers,verbs=update

// HanzoNetworkReconciler reconciles a HanzoNetwork object.
type HanzoNetworkReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoNetwork.
func (r *HanzoNetworkReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzonetwork", req.NamespacedName)

	net := &v1alpha1.HanzoNetwork{}
	if err := r.Get(ctx, req.NamespacedName, net); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoNetwork: %w", err)
	}

	if net.Status.Phase == "" || net.Status.Phase == v1alpha1.PhasePending {
		net.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&net.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := net.Name
	ns := net.Namespace
	headlessSvcName := name + "-validators-headless"

	stdLabels := manifests.StandardLabels(name, "blockchain", "", "")
	valSelectorLabels := manifests.SelectorLabels(name + "-validator")
	allLabels := manifests.MergeLabels(stdLabels, net.Spec.Labels)

	var imagePullSecrets []corev1.LocalObjectReference
	for _, s := range net.Spec.ImagePullSecrets {
		imagePullSecrets = append(imagePullSecrets, corev1.LocalObjectReference{Name: s})
	}

	valImage := net.Spec.Validators.Image.Repository
	if net.Spec.Validators.Image.Tag != "" {
		valImage += ":" + net.Spec.Validators.Image.Tag
	}

	stakingPort := net.Spec.Validators.StakingPort
	if stakingPort == 0 {
		stakingPort = 9651
	}
	httpPort := net.Spec.Validators.HTTPPort
	if httpPort == 0 {
		httpPort = 9650
	}

	bootstrapNodes := strings.Join(net.Spec.Validators.BootstrapNodes, ",")

	valContainer := corev1.Container{
		Name:            "validator",
		Image:           valImage,
		ImagePullPolicy: net.Spec.Validators.Image.PullPolicy,
		Ports: []corev1.ContainerPort{
			{Name: "staking", ContainerPort: stakingPort, Protocol: corev1.ProtocolTCP},
			{Name: "http", ContainerPort: httpPort, Protocol: corev1.ProtocolTCP},
		},
		Env: []corev1.EnvVar{
			{Name: "NETWORK_ID", Value: net.Spec.NetworkID},
			{Name: "BOOTSTRAP_NODES", Value: bootstrapNodes},
		},
		VolumeMounts: []corev1.VolumeMount{
			{Name: "data", MountPath: "/data"},
		},
		ReadinessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{
				TCPSocket: &corev1.TCPSocketAction{Port: intstr.FromInt32(httpPort)},
			},
			InitialDelaySeconds: 15,
			PeriodSeconds:       10,
		},
	}
	if net.Spec.Validators.Resources != nil {
		valContainer.Resources = corev1.ResourceRequirements{
			Requests: net.Spec.Validators.Resources.Requests,
			Limits:   net.Spec.Validators.Resources.Limits,
		}
	}

	var pvcTemplates []corev1.PersistentVolumeClaim
	if net.Spec.Validators.Storage != nil {
		storageClassName := net.Spec.Validators.Storage.StorageClassName
		pvcTemplates = append(pvcTemplates, corev1.PersistentVolumeClaim{
			ObjectMeta: metav1.ObjectMeta{
				Name:   "data",
				Labels: valSelectorLabels,
			},
			Spec: corev1.PersistentVolumeClaimSpec{
				AccessModes:      []corev1.PersistentVolumeAccessMode{corev1.ReadWriteOnce},
				StorageClassName: &storageClassName,
				Resources: corev1.VolumeResourceRequirements{
					Requests: corev1.ResourceList{
						corev1.ResourceStorage: net.Spec.Validators.Storage.Size,
					},
				},
			},
		})
	}

	valLabels := manifests.MergeLabels(allLabels, map[string]string{"app.kubernetes.io/component": "validator"})
	sts := manifests.BuildStatefulSet(
		name+"-validator", ns, valLabels, valSelectorLabels,
		net.Spec.Validators.Replicas,
		[]corev1.Container{valContainer},
		nil, pvcTemplates,
		imagePullSecrets,
		headlessSvcName,
	)
	if err := r.netCreateOrUpdate(ctx, net, sts); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling validator StatefulSet: %w", err)
	}

	// Headless Service.
	valSvcPorts := []corev1.ServicePort{
		{Name: "staking", Port: stakingPort, TargetPort: intstr.FromInt32(stakingPort), Protocol: corev1.ProtocolTCP},
		{Name: "http", Port: httpPort, TargetPort: intstr.FromInt32(httpPort), Protocol: corev1.ProtocolTCP},
	}
	headlessSvc := manifests.BuildHeadlessService(headlessSvcName, ns, valLabels, valSvcPorts, valSelectorLabels)
	if err := r.netCreateOrUpdate(ctx, net, headlessSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling headless Service: %w", err)
	}

	// Regular Service.
	valSvc := manifests.BuildService(name+"-validator", ns, valLabels, valSvcPorts, valSelectorLabels)
	if err := r.netCreateOrUpdate(ctx, net, valSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling validator Service: %w", err)
	}

	// Chain ConfigMaps.
	for _, chain := range net.Spec.Chains {
		chainCM := &corev1.ConfigMap{
			ObjectMeta: metav1.ObjectMeta{
				Name:      name + "-chain-" + chain.Name,
				Namespace: ns,
				Labels:    allLabels,
			},
			Data: map[string]string{
				"genesis.json": chain.Genesis,
				"vmID":         chain.VMID,
				"subnetID":     chain.SubnetID,
			},
		}
		if err := r.netCreateOrUpdate(ctx, net, chainCM); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling chain ConfigMap %s: %w", chain.Name, err)
		}
	}

	// Bootnode.
	if net.Spec.Bootnode != nil && net.Spec.Bootnode.Enabled {
		if err := r.reconcileNetDeployment(ctx, net, name+"-bootnode",
			net.Spec.Bootnode.Image, "ghcr.io/hanzoai/bootnode:latest",
			net.Spec.Bootnode.Resources, allLabels, imagePullSecrets,
			[]corev1.EnvVar{{Name: "NETWORK_ID", Value: net.Spec.NetworkID}},
			9651, "staking"); err != nil {
			return ctrl.Result{}, err
		}
	}

	// Indexer.
	if net.Spec.Indexer != nil && net.Spec.Indexer.Enabled {
		if err := r.reconcileNetDeployment(ctx, net, name+"-indexer",
			net.Spec.Indexer.Image, "ghcr.io/hanzoai/indexer:latest",
			net.Spec.Indexer.Resources, allLabels, imagePullSecrets,
			[]corev1.EnvVar{{Name: "NETWORK_ID", Value: net.Spec.NetworkID}},
			8080, "http"); err != nil {
			return ctrl.Result{}, err
		}
	}

	// Explorer.
	if net.Spec.Explorer != nil && net.Spec.Explorer.Enabled {
		if err := r.reconcileNetDeployment(ctx, net, name+"-explorer",
			net.Spec.Explorer.BackendImage, "ghcr.io/hanzoai/explorer-api:latest",
			net.Spec.Explorer.Resources, allLabels, imagePullSecrets,
			[]corev1.EnvVar{{Name: "NETWORK_ID", Value: net.Spec.NetworkID}},
			8080, "http"); err != nil {
			return ctrl.Result{}, err
		}
	}

	// Bridge.
	if net.Spec.Bridge != nil && net.Spec.Bridge.Enabled {
		if err := r.reconcileNetDeployment(ctx, net, name+"-bridge",
			net.Spec.Bridge.Image, "ghcr.io/hanzoai/bridge:latest",
			net.Spec.Bridge.Resources, allLabels, imagePullSecrets,
			[]corev1.EnvVar{{Name: "NETWORK_ID", Value: net.Spec.NetworkID}},
			8080, "http"); err != nil {
			return ctrl.Result{}, err
		}
	}

	// Update status.
	currentSTS := &appsv1.StatefulSet{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(sts), currentSTS); err != nil {
		return ctrl.Result{}, fmt.Errorf("fetching StatefulSet status: %w", err)
	}

	net.Status.ActiveValidators = currentSTS.Status.ReadyReplicas
	net.Status.ChainCount = int32(len(net.Spec.Chains))
	net.Status.ObservedGeneration = net.Generation

	desiredReplicas := int32(3)
	if net.Spec.Validators.Replicas != nil {
		desiredReplicas = *net.Spec.Validators.Replicas
	}

	if currentSTS.Status.ReadyReplicas >= desiredReplicas && desiredReplicas > 0 {
		net.Status.Phase = v1alpha1.PhaseRunning
		net.Status.BootstrapComplete = true
		status.SetCondition(&net.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available", "All validators are ready")
	} else if currentSTS.Status.ReadyReplicas > 0 {
		net.Status.Phase = v1alpha1.PhaseDegraded
		status.SetCondition(&net.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d validators ready", currentSTS.Status.ReadyReplicas, desiredReplicas))
	} else {
		net.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&net.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No validators are ready yet")
	}

	if err := r.Status().Update(ctx, net); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoNetwork status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", net.Status.Phase)
	return ctrl.Result{}, nil
}

func (r *HanzoNetworkReconciler) reconcileNetDeployment(
	ctx context.Context,
	owner *v1alpha1.HanzoNetwork,
	name, specImage, defaultImage string,
	resources *v1alpha1.ResourceRequirements,
	labels map[string]string,
	imagePullSecrets []corev1.LocalObjectReference,
	env []corev1.EnvVar,
	port int32, portName string,
) error {
	image := specImage
	if image == "" {
		image = defaultImage
	}
	container := corev1.Container{
		Name:  name,
		Image: image,
		Ports: []corev1.ContainerPort{
			{Name: portName, ContainerPort: port, Protocol: corev1.ProtocolTCP},
		},
		Env: env,
	}
	if resources != nil {
		container.Resources = corev1.ResourceRequirements{
			Requests: resources.Requests,
			Limits:   resources.Limits,
		}
	}
	compLabels := manifests.MergeLabels(labels, map[string]string{"app.kubernetes.io/component": name})
	selLabels := manifests.SelectorLabels(name)
	one := int32(1)
	deploy := manifests.BuildDeployment(
		name, owner.Namespace, compLabels, selLabels,
		&one, []corev1.Container{container}, nil,
		v1alpha1.DeploymentStrategyRollingUpdate, imagePullSecrets, "",
	)
	if err := r.netCreateOrUpdate(ctx, owner, deploy); err != nil {
		return fmt.Errorf("reconciling %s Deployment: %w", name, err)
	}
	svcPorts := []corev1.ServicePort{
		{Name: portName, Port: port, TargetPort: intstr.FromInt32(port), Protocol: corev1.ProtocolTCP},
	}
	svc := manifests.BuildService(name, owner.Namespace, compLabels, svcPorts, selLabels)
	if err := r.netCreateOrUpdate(ctx, owner, svc); err != nil {
		return fmt.Errorf("reconciling %s Service: %w", name, err)
	}
	return nil
}

func (r *HanzoNetworkReconciler) netCreateOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoNetwork,
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

// SetupWithManager registers the HanzoNetwork controller with the manager.
func (r *HanzoNetworkReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoNetwork{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.StatefulSet{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&appsv1.Deployment{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&corev1.ConfigMap{}, builder.WithPredicates(updateOrDeletePred())).
		Named("hanzonetwork").
		Complete(r)
}
