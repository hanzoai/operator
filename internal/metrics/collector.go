package metrics

import (
	"github.com/prometheus/client_golang/prometheus"
	"sigs.k8s.io/controller-runtime/pkg/metrics"
)

var (
	// ReconcileTotal counts the total number of reconciliation attempts.
	ReconcileTotal = prometheus.NewCounterVec(
		prometheus.CounterOpts{
			Namespace: "hanzo_operator",
			Name:      "reconcile_total",
			Help:      "Total number of reconciliation attempts by controller and result",
		},
		[]string{"controller", "result"},
	)

	// ReconcileDuration tracks the duration of reconciliation loops.
	ReconcileDuration = prometheus.NewHistogramVec(
		prometheus.HistogramOpts{
			Namespace: "hanzo_operator",
			Name:      "reconcile_duration_seconds",
			Help:      "Duration of reconciliation loops in seconds",
			Buckets:   prometheus.DefBuckets,
		},
		[]string{"controller"},
	)

	// ManagedResources tracks the number of resources managed by each controller.
	ManagedResources = prometheus.NewGaugeVec(
		prometheus.GaugeOpts{
			Namespace: "hanzo_operator",
			Name:      "managed_resources",
			Help:      "Number of Kubernetes resources managed by controller and kind",
		},
		[]string{"controller", "kind"},
	)
)

func init() {
	metrics.Registry.MustRegister(
		ReconcileTotal,
		ReconcileDuration,
		ManagedResources,
	)
}
