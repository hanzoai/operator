// Copyright 2026 Hanzo AI.
// Licensed under the Apache License, Version 2.0.

// Package controller — BaseApp reconciler.
//
// Reconciles BaseApp (hanzo.ai/v1alpha1) into:
//   - StatefulSet with BASE_NODE_ID / BASE_PEERS / BASE_CONSENSUS env
//   - Headless Service <name>-hs for pod DNS (<pod>.<hs>.<ns>.svc...)
//   - ClusterIP Service <name> for gateway round-robin reads
//   - NetworkPolicy (default: allow same-namespace pod-to-pod on BASE port)
//   - Optional KMSSecret CRs
//   - Optional gateway ConfigMap patch with the base_ha upstream binding
//
// Status reports the writer and term from a best-effort read of the
// ClusterIP /_ha/leader endpoint. The authoritative writer tracker lives
// in the gateway — this is just a visibility shim.
package controller

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/go-logr/logr"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/builder"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/predicate"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
	"github.com/hanzoai/operator/internal/manifests"
	"github.com/hanzoai/operator/internal/status"
)

// +kubebuilder:rbac:groups=hanzo.ai,resources=baseapps,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=baseapps/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=baseapps/finalizers,verbs=update
// +kubebuilder:rbac:groups=apps,resources=statefulsets,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services;configmaps,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=persistentvolumeclaims,verbs=get;list;watch
// +kubebuilder:rbac:groups=networking.k8s.io,resources=networkpolicies,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=kms.hanzo.ai,resources=kmssecrets,verbs=get;list;watch;create;update;patch;delete

// BaseAppReconciler reconciles a BaseApp CR.
type BaseAppReconciler struct {
	client.Client
	Scheme     *runtime.Scheme
	Log        logr.Logger
	HTTPClient *http.Client // injected for tests; nil → default short-timeout client
}

// Reconcile implements the reconciliation loop.
func (r *BaseAppReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("baseapp", req.NamespacedName)

	app := &v1alpha1.BaseApp{}
	if err := r.Get(ctx, req.NamespacedName, app); err != nil {
		if errors.IsNotFound(err) {
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching BaseApp: %w", err)
	}

	if app.Status.Phase == "" || app.Status.Phase == v1alpha1.PhasePending {
		status.SetBaseAppPhase(&app.Status, v1alpha1.PhaseCreating)
		status.SetCondition(&app.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	name := app.Name
	ns := app.Namespace
	headlessName := name + "-hs"
	port := app.Spec.Port
	if port == 0 {
		port = 8090
	}
	consensus := app.Spec.Consensus
	if consensus == "" {
		consensus = "quasar"
	}
	replicas := int32(3)
	if app.Spec.Replicas != nil && *app.Spec.Replicas > 0 {
		replicas = *app.Spec.Replicas
	}

	stdLabels := manifests.StandardLabels(name, "baseapp", app.Spec.PartOf, "")
	selectorLabels := manifests.SelectorLabels(name)

	// ---- build env (BASE_* namespace) ----
	baseEnv := buildBaseHAEnv(app, headlessName, replicas, port, consensus)
	baseEnv = append(baseEnv, app.Spec.Env...)

	// ---- image ----
	containerImage := app.Spec.Image.Repository
	if app.Spec.Image.Tag != "" {
		containerImage = containerImage + ":" + app.Spec.Image.Tag
	}
	pullPolicy := corev1.PullIfNotPresent
	if app.Spec.Image.PullPolicy != "" {
		pullPolicy = app.Spec.Image.PullPolicy
	}

	// ---- volumes ----
	var volumes []corev1.Volume
	var mounts []corev1.VolumeMount
	if app.Spec.Schema != "" {
		volumes = append(volumes, corev1.Volume{
			Name: "schema",
			VolumeSource: corev1.VolumeSource{
				ConfigMap: &corev1.ConfigMapVolumeSource{
					LocalObjectReference: corev1.LocalObjectReference{Name: app.Spec.Schema},
				},
			},
		})
		mounts = append(mounts, corev1.VolumeMount{
			Name:      "schema",
			MountPath: "/app/schema",
			ReadOnly:  true,
		})
	}
	mounts = append(mounts, corev1.VolumeMount{
		Name:      "data",
		MountPath: "/app/data",
	})

	// ---- main container ----
	mainContainer := corev1.Container{
		Name:            name,
		Image:           containerImage,
		ImagePullPolicy: pullPolicy,
		Ports: []corev1.ContainerPort{
			{Name: "http", ContainerPort: port, Protocol: corev1.ProtocolTCP},
		},
		Env:          baseEnv,
		EnvFrom:      app.Spec.EnvFrom,
		VolumeMounts: mounts,
		LivenessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{
				HTTPGet: &corev1.HTTPGetAction{
					Path: "/api/health",
					Port: intstr.FromInt32(port),
				},
			},
			InitialDelaySeconds: 20,
			PeriodSeconds:       10,
		},
		ReadinessProbe: &corev1.Probe{
			ProbeHandler: corev1.ProbeHandler{
				HTTPGet: &corev1.HTTPGetAction{
					Path: "/api/health",
					Port: intstr.FromInt32(port),
				},
			},
			InitialDelaySeconds: 5,
			PeriodSeconds:       5,
		},
	}
	if app.Spec.Resources != nil {
		mainContainer.Resources = corev1.ResourceRequirements{
			Requests: app.Spec.Resources.Requests,
			Limits:   app.Spec.Resources.Limits,
		}
	}

	// ---- PVC template ----
	storageClass := app.Spec.Storage.StorageClassName
	pvcTemplates := []corev1.PersistentVolumeClaim{
		{
			ObjectMeta: metav1.ObjectMeta{Name: "data", Labels: selectorLabels},
			Spec: corev1.PersistentVolumeClaimSpec{
				AccessModes:      []corev1.PersistentVolumeAccessMode{corev1.ReadWriteOnce},
				StorageClassName: &storageClass,
				Resources: corev1.VolumeResourceRequirements{
					Requests: corev1.ResourceList{
						corev1.ResourceStorage: app.Spec.Storage.Size,
					},
				},
			},
		},
	}

	sts := manifests.BuildStatefulSet(
		name, ns, stdLabels, selectorLabels,
		&replicas,
		[]corev1.Container{mainContainer},
		volumes, pvcTemplates,
		app.Spec.ImagePullSecrets,
		headlessName,
	)
	if app.Spec.ServiceAccountName != "" {
		sts.Spec.Template.Spec.ServiceAccountName = app.Spec.ServiceAccountName
	}
	if err := r.createOrUpdate(ctx, app, sts); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling StatefulSet: %w", err)
	}

	// ---- services ----
	svcPorts := []corev1.ServicePort{
		{
			Name:       "http",
			Port:       port,
			TargetPort: intstr.FromInt32(port),
			Protocol:   corev1.ProtocolTCP,
		},
	}
	headless := manifests.BuildHeadlessService(headlessName, ns, stdLabels, svcPorts, selectorLabels)
	if err := r.createOrUpdate(ctx, app, headless); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling headless Service: %w", err)
	}
	clusterIP := manifests.BuildService(name, ns, stdLabels, svcPorts, selectorLabels)
	if err := r.createOrUpdate(ctx, app, clusterIP); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Service: %w", err)
	}

	// ---- NetworkPolicy (default intra-namespace on http port) ----
	np := r.buildNetworkPolicy(app, selectorLabels, stdLabels, port)
	if np != nil {
		if err := r.createOrUpdate(ctx, app, np); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling NetworkPolicy: %w", err)
		}
	}

	// ---- KMSSecret CRs ----
	for i := range app.Spec.KMSSecrets {
		kmsRef := &app.Spec.KMSSecrets[i]
		if err := r.reconcileKMSSecret(ctx, app, kmsRef, stdLabels); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling KMSSecret %s: %w", kmsRef.ManagedSecretName, err)
		}
	}

	// ---- Gateway ConfigMap patch (best-effort) ----
	if app.Spec.Gateway != nil && app.Spec.Gateway.Route != "" {
		if err := r.reconcileGatewayRoute(ctx, app, port); err != nil {
			log.Info("gateway route patch failed (non-fatal)", "err", err.Error())
			// Non-fatal — operators may manage gateway config directly.
		}
	}

	// ---- status ----
	currentSTS := &appsv1.StatefulSet{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(sts), currentSTS); err != nil {
		return ctrl.Result{}, fmt.Errorf("fetching StatefulSet status: %w", err)
	}
	app.Status.ReadyReplicas = currentSTS.Status.ReadyReplicas
	app.Status.ObservedGeneration = app.Generation

	if currentSTS.Status.ReadyReplicas >= replicas && replicas > 0 {
		status.SetBaseAppPhase(&app.Status, v1alpha1.PhaseRunning)
		status.SetCondition(&app.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available", "All replicas ready")
		// Best-effort leader read to populate status.
		if w, term := r.fetchLeader(name, ns, port); w != "" {
			app.Status.CurrentWriter = w
			app.Status.Term = term
		}
	} else if currentSTS.Status.ReadyReplicas > 0 {
		status.SetBaseAppPhase(&app.Status, v1alpha1.PhaseDegraded)
		status.SetCondition(&app.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d replicas ready", currentSTS.Status.ReadyReplicas, replicas))
	} else {
		status.SetBaseAppPhase(&app.Status, v1alpha1.PhaseCreating)
		status.SetCondition(&app.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No replicas ready yet")
	}

	if err := r.Status().Update(ctx, app); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating BaseApp status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", app.Status.Phase, "writer", app.Status.CurrentWriter)
	// Requeue briefly when degraded so we detect failover.
	if app.Status.Phase == v1alpha1.PhaseDegraded || app.Status.CurrentWriter == "" {
		return ctrl.Result{RequeueAfter: 10 * time.Second}, nil
	}
	return ctrl.Result{RequeueAfter: 60 * time.Second}, nil
}

// buildBaseHAEnv emits the BASE_* env vars every base-ha pod requires.
// Pod ordinals resolve from $HOSTNAME (StatefulSet guarantees pod-{N}).
// Peer addresses are expressed in pod-DNS form so SRV discovery is not
// required.
func buildBaseHAEnv(app *v1alpha1.BaseApp, headlessName string, replicas, port int32, consensus string) []corev1.EnvVar {
	name := app.Name
	ns := app.Namespace
	peers := make([]string, 0, replicas)
	for i := int32(0); i < replicas; i++ {
		peers = append(peers, fmt.Sprintf("http://%s-%d.%s.%s.svc.cluster.local:%d",
			name, i, headlessName, ns, port))
	}

	out := []corev1.EnvVar{
		{Name: "BASE_NODE_ID", ValueFrom: &corev1.EnvVarSource{
			FieldRef: &corev1.ObjectFieldSelector{FieldPath: "metadata.name"},
		}},
		{Name: "BASE_CONSENSUS", Value: consensus},
		{Name: "BASE_PEERS", Value: strings.Join(peers, ",")},
		{Name: "BASE_LOCAL_TARGET", Value: fmt.Sprintf("http://$(BASE_NODE_ID).%s.%s.svc.cluster.local:%d",
			headlessName, ns, port)},
	}
	if app.Spec.IAMApp != "" {
		out = append(out, corev1.EnvVar{Name: "IAM_APP", Value: app.Spec.IAMApp})
	}
	return out
}

// buildNetworkPolicy emits a default NetworkPolicy allowing (a) same-
// namespace pod-to-pod traffic on the HTTP port (for pod-to-pod HA
// heartbeat + replication), and (b) whatever else the user specifies
// via app.Spec.NetworkPolicy.
func (r *BaseAppReconciler) buildNetworkPolicy(
	app *v1alpha1.BaseApp,
	selectorLabels, stdLabels map[string]string,
	port int32,
) *networkingv1.NetworkPolicy {
	if app.Spec.NetworkPolicy != nil && app.Spec.NetworkPolicy.Enabled != nil && !*app.Spec.NetworkPolicy.Enabled {
		return nil
	}
	spec := app.Spec.NetworkPolicy
	if spec == nil {
		spec = &v1alpha1.NetworkPolicySpec{}
	}
	np := manifests.BuildNetworkPolicy(app.Name, app.Namespace, spec, selectorLabels, stdLabels)
	// Restrict the ingress rule to the HTTP port so we don't expose
	// arbitrary in-pod ports to the namespace.
	tcpProto := corev1.ProtocolTCP
	portIntStr := intstr.FromInt32(port)
	if len(np.Spec.Ingress) > 0 {
		np.Spec.Ingress[0].Ports = []networkingv1.NetworkPolicyPort{
			{Protocol: &tcpProto, Port: &portIntStr},
		}
	}
	return np
}

// reconcileGatewayRoute patches the target gateway ConfigMap (if present)
// with a backend binding of kind base_ha. When the ConfigMap is not
// found, the operator logs and moves on — gateways may be managed
// externally.
func (r *BaseAppReconciler) reconcileGatewayRoute(ctx context.Context, app *v1alpha1.BaseApp, port int32) error {
	g := app.Spec.Gateway
	gwName := g.GatewayName
	if gwName == "" {
		gwName = "gateway"
	}
	gwNs := g.GatewayNamespace
	if gwNs == "" {
		gwNs = app.Namespace
	}
	cm := &corev1.ConfigMap{}
	if err := r.Get(ctx, client.ObjectKey{Namespace: gwNs, Name: gwName}, cm); err != nil {
		return fmt.Errorf("gateway ConfigMap %s/%s: %w", gwNs, gwName, err)
	}
	key := "gateway.json"
	raw, ok := cm.Data[key]
	if !ok {
		return fmt.Errorf("gateway ConfigMap has no %q key", key)
	}
	var cfg map[string]any
	if err := json.Unmarshal([]byte(raw), &cfg); err != nil {
		return fmt.Errorf("parse gateway.json: %w", err)
	}

	svcDNS := fmt.Sprintf("%s.%s.svc.cluster.local", app.Name, app.Namespace)
	poll := g.LeaderPollInterval
	if poll == "" {
		poll = "1s"
	}
	ryw := g.ReadYourWritesTTL
	if ryw == "" {
		ryw = "5s"
	}

	endpoint := map[string]any{
		"endpoint":       g.Route,
		"method":         "GET",
		"output_encoding": "no-op",
		"backend": []map[string]any{
			{
				"url_pattern": g.Route,
				"host":        []string{fmt.Sprintf("http://%s:%d", svcDNS, port)},
				"extra_config": map[string]any{
					"github.com/hanzoai/gateway/base_ha": map[string]any{
						"service_dns":          svcDNS,
						"port":                 port,
						"leader_poll_interval": poll,
						"read_your_writes_ttl": ryw,
					},
				},
			},
		},
	}
	eps, _ := cfg["endpoints"].([]any)
	// Replace an existing endpoint with the same URL pattern, otherwise
	// append. This keeps the patch idempotent across reconciles.
	replaced := false
	for i, e := range eps {
		m, ok := e.(map[string]any)
		if !ok {
			continue
		}
		if m["endpoint"] == g.Route {
			eps[i] = endpoint
			replaced = true
			break
		}
	}
	if !replaced {
		eps = append(eps, endpoint)
	}
	cfg["endpoints"] = eps

	patched, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal patched gateway.json: %w", err)
	}
	if string(patched) == raw {
		return nil // no-op
	}
	cm.Data[key] = string(patched)
	return r.Update(ctx, cm)
}

// fetchLeader reads GET http://<name>.<ns>.svc.cluster.local:<port>/_ha/leader
// with a tight timeout. Failures are non-fatal — they just leave status
// empty.
func (r *BaseAppReconciler) fetchLeader(name, ns string, port int32) (string, uint64) {
	u := fmt.Sprintf("http://%s.%s.svc.cluster.local:%d/_ha/leader", name, ns, port)
	c := r.HTTPClient
	if c == nil {
		c = &http.Client{Timeout: 2 * time.Second}
	}
	req, err := http.NewRequest(http.MethodGet, u, nil)
	if err != nil {
		return "", 0
	}
	resp, err := c.Do(req)
	if err != nil {
		return "", 0
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", 0
	}
	var body struct {
		LeaderURL string `json:"leader_url"`
		Term      uint64 `json:"term"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, 4096)).Decode(&body); err != nil {
		return "", 0
	}
	return body.LeaderURL, body.Term
}

// createOrUpdate writes a managed resource idempotently with owner ref.
func (r *BaseAppReconciler) createOrUpdate(ctx context.Context, owner *v1alpha1.BaseApp, desired client.Object) error {
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

// reconcileKMSSecret mirrors the HanzoDatastore pattern.
func (r *BaseAppReconciler) reconcileKMSSecret(
	ctx context.Context,
	owner *v1alpha1.BaseApp,
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
	_, err := ctrl.CreateOrUpdate(ctx, r.Client, existing, func() error {
		desiredSpec, _, _ := unstructured.NestedMap(kms.Object, "spec")
		if desiredSpec != nil {
			if err := unstructured.SetNestedMap(existing.Object, desiredSpec, "spec"); err != nil {
				return err
			}
		}
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
	return err
}

// SetupWithManager registers the BaseApp controller.
func (r *BaseAppReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.BaseApp{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.StatefulSet{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&networkingv1.NetworkPolicy{}, builder.WithPredicates(updateOrDeletePred())).
		Named("baseapp").
		Complete(r)
}
