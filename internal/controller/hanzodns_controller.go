package controller

import (
	"context"
	"fmt"

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

	"github.com/go-logr/logr"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
	"github.com/hanzoai/operator/internal/manifests"
	"github.com/hanzoai/operator/internal/status"
)

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzodns,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzodns/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzodns/finalizers,verbs=update

// HanzoDNSReconciler reconciles a HanzoDNS object.
type HanzoDNSReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoDNS.
func (r *HanzoDNSReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzodns", req.NamespacedName)

	dns := &v1alpha1.HanzoDNS{}
	if err := r.Get(ctx, req.NamespacedName, dns); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoDNS: %w", err)
	}

	if dns.Status.Phase == "" || dns.Status.Phase == v1alpha1.PhasePending {
		dns.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&dns.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := dns.Name
	ns := dns.Namespace

	stdLabels := manifests.StandardLabels(name, "dns", "", "")
	allLabels := manifests.MergeLabels(stdLabels, dns.Spec.Labels)
	selLabels := selectorLabels(name, "dns")

	// Reconcile CoreDNS ConfigMap.
	corefile := r.buildCorefile(dns)
	cm := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name + "-corefile",
			Namespace: ns,
			Labels:    allLabels,
		},
		Data: map[string]string{
			"Corefile": corefile,
		},
	}
	if err := r.createOrUpdate(ctx, dns, cm); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling ConfigMap: %w", err)
	}

	// Reconcile CoreDNS Deployment.
	replicas := int32(2)
	image := "ghcr.io/hanzoai/dns:latest"
	apiPort := int32(8443)
	dnsPort := int32(53)

	if dns.Spec.CoreDNS != nil {
		if dns.Spec.CoreDNS.Replicas != nil {
			replicas = *dns.Spec.CoreDNS.Replicas
		}
		if dns.Spec.CoreDNS.Image != "" {
			image = dns.Spec.CoreDNS.Image
		}
		if dns.Spec.CoreDNS.APIPort > 0 {
			apiPort = dns.Spec.CoreDNS.APIPort
		}
		if dns.Spec.CoreDNS.DNSPort > 0 {
			dnsPort = dns.Spec.CoreDNS.DNSPort
		}
	}

	dep := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name + "-coredns",
			Namespace: ns,
			Labels:    allLabels,
		},
		Spec: appsv1.DeploymentSpec{
			Replicas: &replicas,
			Selector: &metav1.LabelSelector{MatchLabels: selLabels},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: allLabels},
				Spec: corev1.PodSpec{
					Containers: []corev1.Container{
						{
							Name:  "coredns",
							Image: image,
							Args:  []string{"-conf", "/etc/coredns/Corefile"},
							Ports: []corev1.ContainerPort{
								{Name: "dns-tcp", ContainerPort: dnsPort, Protocol: corev1.ProtocolTCP},
								{Name: "dns-udp", ContainerPort: dnsPort, Protocol: corev1.ProtocolUDP},
								{Name: "api", ContainerPort: apiPort, Protocol: corev1.ProtocolTCP},
							},
							ReadinessProbe: &corev1.Probe{
								ProbeHandler: corev1.ProbeHandler{
									HTTPGet: &corev1.HTTPGetAction{
										Path: "/health",
										Port: intstr.FromInt32(8080),
									},
								},
								InitialDelaySeconds: 5,
								PeriodSeconds:       10,
							},
							LivenessProbe: &corev1.Probe{
								ProbeHandler: corev1.ProbeHandler{
									HTTPGet: &corev1.HTTPGetAction{
										Path: "/health",
										Port: intstr.FromInt32(8080),
									},
								},
								InitialDelaySeconds: 10,
								PeriodSeconds:       30,
							},
							VolumeMounts: []corev1.VolumeMount{
								{Name: "corefile", MountPath: "/etc/coredns", ReadOnly: true},
							},
						},
					},
					Volumes: []corev1.Volume{
						{
							Name: "corefile",
							VolumeSource: corev1.VolumeSource{
								ConfigMap: &corev1.ConfigMapVolumeSource{
									LocalObjectReference: corev1.LocalObjectReference{Name: name + "-corefile"},
								},
							},
						},
					},
				},
			},
		},
	}

	// Apply resource requirements if specified.
	if dns.Spec.CoreDNS != nil && dns.Spec.CoreDNS.Resources != nil {
		dep.Spec.Template.Spec.Containers[0].Resources = corev1.ResourceRequirements{
			Requests: dns.Spec.CoreDNS.Resources.Requests,
			Limits:   dns.Spec.CoreDNS.Resources.Limits,
		}
	}

	// Set env vars for OIDC if configured.
	if dns.Spec.OIDC != nil && dns.Spec.OIDC.Enabled {
		envs := []corev1.EnvVar{
			{Name: "HANZO_DNS_OIDC_ISSUER", Value: dns.Spec.OIDC.Issuer},
		}
		if dns.Spec.OIDC.Audience != "" {
			envs = append(envs, corev1.EnvVar{Name: "HANZO_DNS_OIDC_AUDIENCE", Value: dns.Spec.OIDC.Audience})
		}
		dep.Spec.Template.Spec.Containers[0].Env = append(dep.Spec.Template.Spec.Containers[0].Env, envs...)
	}

	if err := r.createOrUpdate(ctx, dns, dep); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Deployment: %w", err)
	}

	// Reconcile DNS Service (UDP+TCP).
	dnsSvc := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name + "-dns",
			Namespace: ns,
			Labels:    allLabels,
		},
		Spec: corev1.ServiceSpec{
			Selector: selLabels,
			Ports: []corev1.ServicePort{
				{Name: "dns-tcp", Port: 53, TargetPort: intstr.FromString("dns-tcp"), Protocol: corev1.ProtocolTCP},
				{Name: "dns-udp", Port: 53, TargetPort: intstr.FromString("dns-udp"), Protocol: corev1.ProtocolUDP},
			},
		},
	}
	if err := r.createOrUpdate(ctx, dns, dnsSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling DNS Service: %w", err)
	}

	// Reconcile API Service.
	apiSvc := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name + "-api",
			Namespace: ns,
			Labels:    allLabels,
		},
		Spec: corev1.ServiceSpec{
			Selector: selLabels,
			Ports: []corev1.ServicePort{
				{Name: "api", Port: 80, TargetPort: intstr.FromString("api"), Protocol: corev1.ProtocolTCP},
			},
		},
	}
	if err := r.createOrUpdate(ctx, dns, apiSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling API Service: %w", err)
	}

	// Update status.
	dns.Status.ManagedZones = int32(len(dns.Spec.Zones))
	dns.Status.ObservedGeneration = dns.Generation
	dns.Status.CoreDNSReady = true // Will be enhanced with actual readiness check.
	dns.Status.ZoneStatuses = make([]v1alpha1.ZoneSyncStatus, 0, len(dns.Spec.Zones))
	for _, z := range dns.Spec.Zones {
		dns.Status.ZoneStatuses = append(dns.Status.ZoneStatuses, v1alpha1.ZoneSyncStatus{
			Name:          z.Name,
			CoreDNSSynced: true,
			CloudflareSynced: dns.Spec.Cloudflare != nil && dns.Spec.Cloudflare.Enabled &&
				z.CloudflareZoneID != "",
		})
	}

	dns.Status.Phase = v1alpha1.PhaseRunning
	status.SetCondition(&dns.Status.Conditions, v1alpha1.ConditionTypeReady,
		metav1.ConditionTrue, "Available",
		fmt.Sprintf("CoreDNS running with %d zones", dns.Status.ManagedZones))

	if err := r.Status().Update(ctx, dns); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoDNS status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", dns.Status.Phase, "zones", dns.Status.ManagedZones)
	return ctrl.Result{}, nil
}

// buildCorefile generates a CoreDNS Corefile for the managed zones.
func (r *HanzoDNSReconciler) buildCorefile(dns *v1alpha1.HanzoDNS) string {
	apiPort := int32(8443)
	if dns.Spec.CoreDNS != nil && dns.Spec.CoreDNS.APIPort > 0 {
		apiPort = dns.Spec.CoreDNS.APIPort
	}

	corefile := fmt.Sprintf(`. {
    hanzodns :%d
    health :8080
    ready :8181
    prometheus :9153
    log
    errors
}
`, apiPort)

	return corefile
}

func (r *HanzoDNSReconciler) createOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoDNS,
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

// SetupWithManager registers the HanzoDNS controller with the manager.
func (r *HanzoDNSReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoDNS{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.Deployment{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&corev1.ConfigMap{}, builder.WithPredicates(updateOrDeletePred())).
		Named("hanzodns").
		Complete(r)
}
