package controller

import (
	"context"
	"fmt"

	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	"github.com/go-logr/logr"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
	"github.com/hanzoai/operator/internal/manifests"
	"github.com/hanzoai/operator/internal/status"
)

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoingresses,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoingresses/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzoingresses/finalizers,verbs=update

// HanzoIngressReconciler reconciles a HanzoIngress object.
type HanzoIngressReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoIngress.
func (r *HanzoIngressReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzoingress", req.NamespacedName)

	hi := &v1alpha1.HanzoIngress{}
	if err := r.Get(ctx, req.NamespacedName, hi); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoIngress: %w", err)
	}

	if hi.Status.Phase == "" || hi.Status.Phase == v1alpha1.PhasePending {
		hi.Status.Phase = v1alpha1.PhaseCreating
		status.SetCondition(&hi.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := hi.Name
	ns := hi.Namespace

	stdLabels := manifests.StandardLabels(name, "ingress", "", "")
	allLabels := manifests.MergeLabels(stdLabels, hi.Spec.Labels)

	pathType := networkingv1.PathTypePrefix
	managedCount := int32(0)

	for _, domain := range hi.Spec.Domains {
		ingName := name + "-" + sanitizeDNS(domain.Domain)

		annotations := make(map[string]string)
		if domain.TLS {
			annotations["cert-manager.io/cluster-issuer"] = hi.Spec.ClusterIssuer
		}
		for k, v := range hi.Spec.Annotations {
			annotations[k] = v
		}

		var paths []networkingv1.HTTPIngressPath
		for _, route := range domain.Routes {
			pt := pathType
			switch route.PathType {
			case "Exact":
				pt = networkingv1.PathTypeExact
			case "ImplementationSpecific":
				pt = networkingv1.PathTypeImplementationSpecific
			}
			paths = append(paths, networkingv1.HTTPIngressPath{
				Path:     route.Path,
				PathType: &pt,
				Backend: networkingv1.IngressBackend{
					Service: &networkingv1.IngressServiceBackend{
						Name: route.ServiceName,
						Port: networkingv1.ServiceBackendPort{Number: route.ServicePort},
					},
				},
			})
		}

		rules := []networkingv1.IngressRule{
			{
				Host: domain.Domain,
				IngressRuleValue: networkingv1.IngressRuleValue{
					HTTP: &networkingv1.HTTPIngressRuleValue{Paths: paths},
				},
			},
		}

		var tls []networkingv1.IngressTLS
		if domain.TLS {
			tls = append(tls, networkingv1.IngressTLS{
				Hosts:      []string{domain.Domain},
				SecretName: ingName + "-tls",
			})
		}

		ing := &networkingv1.Ingress{
			ObjectMeta: metav1.ObjectMeta{
				Name:        ingName,
				Namespace:   ns,
				Labels:      allLabels,
				Annotations: annotations,
			},
			Spec: networkingv1.IngressSpec{
				Rules: rules,
				TLS:   tls,
			},
		}
		if hi.Spec.IngressClassName != "" {
			ing.Spec.IngressClassName = &hi.Spec.IngressClassName
		}

		if err := r.ingressCreateOrUpdate(ctx, hi, ing); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling Ingress for %s: %w", domain.Domain, err)
		}
		managedCount++
	}

	// Update status.
	hi.Status.ManagedIngresses = managedCount
	hi.Status.ObservedGeneration = hi.Generation
	hi.Status.CertificateStatuses = make([]v1alpha1.CertificateStatus, 0, len(hi.Spec.Domains))
	for _, domain := range hi.Spec.Domains {
		cs := v1alpha1.CertificateStatus{
			Domain:  domain.Domain,
			Ready:   !domain.TLS,
			Message: "Pending",
		}
		if domain.TLS {
			cs.Message = "Certificate managed by cert-manager"
		}
		hi.Status.CertificateStatuses = append(hi.Status.CertificateStatuses, cs)
	}

	hi.Status.Phase = v1alpha1.PhaseRunning
	status.SetCondition(&hi.Status.Conditions, v1alpha1.ConditionTypeReady,
		metav1.ConditionTrue, "Available",
		fmt.Sprintf("%d ingress resources managed", managedCount))

	if err := r.Status().Update(ctx, hi); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoIngress status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", hi.Status.Phase, "ingresses", managedCount)
	return ctrl.Result{}, nil
}

func sanitizeDNS(domain string) string {
	out := make([]byte, 0, len(domain))
	for _, c := range []byte(domain) {
		if (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' {
			out = append(out, c)
		} else if c >= 'A' && c <= 'Z' {
			out = append(out, c+32)
		} else {
			out = append(out, '-')
		}
	}
	if len(out) > 63 {
		out = out[:63]
	}
	return string(out)
}

func (r *HanzoIngressReconciler) ingressCreateOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoIngress,
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

// SetupWithManager registers the HanzoIngress controller with the manager.
func (r *HanzoIngressReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoIngress{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&networkingv1.Ingress{}, builder.WithPredicates(updateOrDeletePred())).
		Named("hanzoingress").
		Complete(r)
}
