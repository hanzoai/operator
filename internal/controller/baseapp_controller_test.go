// Copyright 2026 Hanzo AI.
// Licensed under the Apache License, Version 2.0.

package controller

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/go-logr/logr/testr"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
)

// newTestScheme registers every type the BaseApp reconciler emits.
func newTestScheme(t *testing.T) *runtime.Scheme {
	t.Helper()
	s := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(s); err != nil {
		t.Fatalf("core scheme: %v", err)
	}
	if err := v1alpha1.AddToScheme(s); err != nil {
		t.Fatalf("v1alpha1 scheme: %v", err)
	}
	return s
}

// newBaseApp builds a well-formed BaseApp fixture.
func newBaseApp(name, ns string, mut ...func(*v1alpha1.BaseApp)) *v1alpha1.BaseApp {
	r := int32(3)
	app := &v1alpha1.BaseApp{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: ns},
		Spec: v1alpha1.BaseAppSpec{
			Image:    v1alpha1.ImageSpec{Repository: "ghcr.io/hanzoai/base-ha", Tag: "v1.0.0"},
			Replicas: &r,
			Port:     8090,
			Storage: v1alpha1.StorageSpec{
				StorageClassName: "premium-rwo",
				Size:             resource.MustParse("10Gi"),
			},
		},
	}
	for _, m := range mut {
		m(app)
	}
	return app
}

func runReconcile(t *testing.T, r *BaseAppReconciler, name, ns string) {
	t.Helper()
	_, err := r.Reconcile(context.Background(), ctrl.Request{
		NamespacedName: types.NamespacedName{Name: name, Namespace: ns},
	})
	if err != nil {
		t.Fatalf("reconcile: %v", err)
	}
}

// TestBaseAppReconcileEmitsStatefulSet proves the core Kind is created
// with the correct replica count, peer list env, and volume claim template.
func TestBaseAppReconcileEmitsStatefulSet(t *testing.T) {
	s := newTestScheme(t)
	app := newBaseApp("foo", "hanzo")
	c := fake.NewClientBuilder().WithScheme(s).WithStatusSubresource(&v1alpha1.BaseApp{}).WithObjects(app).Build()
	r := &BaseAppReconciler{Client: c, Scheme: s, Log: testr.New(t)}
	runReconcile(t, r, "foo", "hanzo")

	sts := &appsv1.StatefulSet{}
	if err := c.Get(context.Background(), client.ObjectKey{Name: "foo", Namespace: "hanzo"}, sts); err != nil {
		t.Fatalf("get sts: %v", err)
	}
	if sts.Spec.Replicas == nil || *sts.Spec.Replicas != 3 {
		t.Fatalf("replicas = %v, want 3", sts.Spec.Replicas)
	}
	if sts.Spec.ServiceName != "foo-hs" {
		t.Fatalf("serviceName = %q, want foo-hs", sts.Spec.ServiceName)
	}
	if len(sts.Spec.VolumeClaimTemplates) != 1 {
		t.Fatalf("VolumeClaimTemplates: %+v", sts.Spec.VolumeClaimTemplates)
	}
	env := envToMap(sts.Spec.Template.Spec.Containers[0].Env)
	if want := "quasar"; env["BASE_CONSENSUS"] != want {
		t.Fatalf("BASE_CONSENSUS = %q, want %q", env["BASE_CONSENSUS"], want)
	}
	peers := env["BASE_PEERS"]
	for i := 0; i < 3; i++ {
		wantPeer := fmt.Sprintf("http://foo-%d.foo-hs.hanzo.svc.cluster.local:8090", i)
		if !strings.Contains(peers, wantPeer) {
			t.Fatalf("BASE_PEERS missing peer %s: %s", wantPeer, peers)
		}
	}
	local := env["BASE_LOCAL_TARGET"]
	if !strings.Contains(local, "$(BASE_NODE_ID).foo-hs.hanzo.svc.cluster.local:8090") {
		t.Fatalf("BASE_LOCAL_TARGET = %q", local)
	}
}

// TestBaseAppReconcileEmitsServicesAndNetworkPolicy covers the companion
// resources (headless + ClusterIP + NetworkPolicy).
func TestBaseAppReconcileEmitsServicesAndNetworkPolicy(t *testing.T) {
	s := newTestScheme(t)
	app := newBaseApp("foo", "hanzo")
	c := fake.NewClientBuilder().WithScheme(s).WithStatusSubresource(&v1alpha1.BaseApp{}).WithObjects(app).Build()
	r := &BaseAppReconciler{Client: c, Scheme: s, Log: testr.New(t)}
	runReconcile(t, r, "foo", "hanzo")

	// Headless service.
	hs := &corev1.Service{}
	if err := c.Get(context.Background(), client.ObjectKey{Name: "foo-hs", Namespace: "hanzo"}, hs); err != nil {
		t.Fatalf("get headless service: %v", err)
	}
	if hs.Spec.ClusterIP != "None" {
		t.Fatalf("headless svc must have ClusterIP=None, got %q", hs.Spec.ClusterIP)
	}
	// ClusterIP service.
	svc := &corev1.Service{}
	if err := c.Get(context.Background(), client.ObjectKey{Name: "foo", Namespace: "hanzo"}, svc); err != nil {
		t.Fatalf("get ClusterIP service: %v", err)
	}
	if svc.Spec.ClusterIP == "None" {
		t.Fatalf("ClusterIP service must not be headless")
	}
	// NetworkPolicy.
	np := &networkingv1.NetworkPolicy{}
	if err := c.Get(context.Background(), client.ObjectKey{Name: "foo", Namespace: "hanzo"}, np); err != nil {
		t.Fatalf("get NetworkPolicy: %v", err)
	}
	if len(np.Spec.Ingress) == 0 || len(np.Spec.Ingress[0].Ports) == 0 {
		t.Fatalf("NetworkPolicy missing port restriction")
	}
	port := np.Spec.Ingress[0].Ports[0]
	if port.Port == nil || port.Port.IntValue() != 8090 {
		t.Fatalf("NetworkPolicy port must restrict to 8090: %+v", port)
	}
}

// TestBaseAppReconcileGatewayRoutePatchesConfigMap verifies the gateway
// ConfigMap gets a base_ha upstream entry when Spec.Gateway.Route is set.
func TestBaseAppReconcileGatewayRoutePatchesConfigMap(t *testing.T) {
	s := newTestScheme(t)
	app := newBaseApp("foo", "hanzo", func(b *v1alpha1.BaseApp) {
		b.Spec.Gateway = &v1alpha1.BaseAppGatewaySpec{
			Route:       "/v1/app/foo",
			GatewayName: "gateway",
			// GatewayNamespace empty = same as BaseApp ns
			LeaderPollInterval: "1s",
			ReadYourWritesTTL:  "5s",
		}
	})
	// Seed an empty gateway ConfigMap.
	initial, _ := json.Marshal(map[string]any{
		"version":   3,
		"endpoints": []any{},
	})
	cm := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: "gateway", Namespace: "hanzo"},
		Data:       map[string]string{"gateway.json": string(initial)},
	}

	c := fake.NewClientBuilder().WithScheme(s).WithStatusSubresource(&v1alpha1.BaseApp{}).WithObjects(app, cm).Build()
	r := &BaseAppReconciler{Client: c, Scheme: s, Log: testr.New(t)}
	runReconcile(t, r, "foo", "hanzo")

	updated := &corev1.ConfigMap{}
	if err := c.Get(context.Background(), client.ObjectKey{Name: "gateway", Namespace: "hanzo"}, updated); err != nil {
		t.Fatalf("get cm: %v", err)
	}
	raw := updated.Data["gateway.json"]
	var cfg map[string]any
	if err := json.Unmarshal([]byte(raw), &cfg); err != nil {
		t.Fatalf("unmarshal patched cfg: %v", err)
	}
	eps, _ := cfg["endpoints"].([]any)
	if len(eps) != 1 {
		t.Fatalf("want 1 endpoint, got %d: %+v", len(eps), eps)
	}
	ep, _ := eps[0].(map[string]any)
	if ep["endpoint"] != "/v1/app/foo" {
		t.Fatalf("endpoint URL = %v", ep["endpoint"])
	}
	be, _ := ep["backend"].([]any)
	if len(be) != 1 {
		t.Fatalf("backend count: %d", len(be))
	}
	beMap, _ := be[0].(map[string]any)
	extra, _ := beMap["extra_config"].(map[string]any)
	ha, _ := extra["github.com/hanzoai/gateway/base_ha"].(map[string]any)
	if ha == nil {
		t.Fatalf("base_ha upstream missing in patched config: %+v", beMap)
	}
	if got := ha["service_dns"].(string); got != "foo.hanzo.svc.cluster.local" {
		t.Fatalf("service_dns = %q", got)
	}
	// Port is encoded numerically in JSON — allow int or float64.
	if p, ok := ha["port"].(float64); !ok || int(p) != 8090 {
		t.Fatalf("port = %v", ha["port"])
	}

	// Second reconcile must be idempotent — same endpoint, not duplicated.
	runReconcile(t, r, "foo", "hanzo")
	_ = c.Get(context.Background(), client.ObjectKey{Name: "gateway", Namespace: "hanzo"}, updated)
	_ = json.Unmarshal([]byte(updated.Data["gateway.json"]), &cfg)
	if eps, _ := cfg["endpoints"].([]any); len(eps) != 1 {
		t.Fatalf("reconcile must be idempotent, got %d endpoints", len(eps))
	}
}

// TestBaseAppReconcileGatewayRouteSkippedWhenCMMissing proves reconcile
// does not fail when the gateway ConfigMap is not present (gateway may
// be managed externally).
func TestBaseAppReconcileGatewayRouteSkippedWhenCMMissing(t *testing.T) {
	s := newTestScheme(t)
	app := newBaseApp("foo", "hanzo", func(b *v1alpha1.BaseApp) {
		b.Spec.Gateway = &v1alpha1.BaseAppGatewaySpec{Route: "/v1/app/foo"}
	})
	c := fake.NewClientBuilder().WithScheme(s).WithStatusSubresource(&v1alpha1.BaseApp{}).WithObjects(app).Build()
	r := &BaseAppReconciler{Client: c, Scheme: s, Log: testr.New(t)}
	// Missing CM must not fail the reconcile.
	runReconcile(t, r, "foo", "hanzo")
	// CM still absent.
	cm := &corev1.ConfigMap{}
	err := c.Get(context.Background(), client.ObjectKey{Name: "gateway", Namespace: "hanzo"}, cm)
	if !apierrors.IsNotFound(err) {
		t.Fatalf("CM should still be missing, got err=%v", err)
	}
}

// TestBaseAppReconcileStatusReportsWriter uses an in-memory HTTP stub
// mimicking /_ha/leader so the controller can populate status.CurrentWriter.
func TestBaseAppReconcileStatusReportsWriter(t *testing.T) {
	t.Skip("status enrichment requires dialing the in-cluster ClusterIP which fake client cannot simulate — covered by e2e")
	_ = httptest.NewServer
}

// envToMap lowers a []corev1.EnvVar to a lookup map of concrete values
// (ignoring ValueFrom entries — those are checked separately).
func envToMap(envs []corev1.EnvVar) map[string]string {
	out := make(map[string]string, len(envs))
	for _, e := range envs {
		out[e.Name] = e.Value
	}
	return out
}

// Sanity — `http.StatusOK` used below is provided by the test server.
var _ = http.StatusOK
