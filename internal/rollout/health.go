package rollout

import (
	"context"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"time"
)

// HealthPath returns the health endpoint path for a service.
func HealthPath(name string) string {
	if IsBaseService(name) {
		return "/v1/base/health"
	}
	return "/healthz"
}

// HealthPort returns the container port to check.
func HealthPort(name string) int {
	switch name {
	case "ats", "bd", "ta":
		return 8090
	case "iam":
		return 8000
	case "kms":
		return 8443
	case "gateway":
		return 8080
	case "exchange", "superadmin", "id":
		return 3000
	case "aml":
		return 5001
	case "mm":
		return 8080
	default:
		return 8080
	}
}

// CheckHealth performs a health check against a service via port-forward.
// Returns nil if healthy, error otherwise.
func CheckHealth(ctx context.Context, kubeCtx, namespace, kind, name string, timeout time.Duration, retries int) error {
	port := HealthPort(name)
	path := HealthPath(name)

	addr, cancel, err := PortForward(ctx, kubeCtx, namespace, kind, name, port)
	if err != nil {
		return fmt.Errorf("health check setup for %s: %w", name, err)
	}
	defer cancel()

	// Give port-forward a moment to stabilize
	time.Sleep(500 * time.Millisecond)

	url := addr + path
	client := &http.Client{Timeout: 10 * time.Second}

	var lastErr error
	for attempt := 1; attempt <= retries; attempt++ {
		reqCtx, reqCancel := context.WithTimeout(ctx, timeout)
		req, err := http.NewRequestWithContext(reqCtx, http.MethodGet, url, nil)
		if err != nil {
			reqCancel()
			return fmt.Errorf("create health request: %w", err)
		}

		resp, err := client.Do(req)
		reqCancel()
		if err != nil {
			lastErr = fmt.Errorf("attempt %d: %w", attempt, err)
			slog.Warn("health check failed", "service", name, "attempt", attempt, "error", err)
			time.Sleep(5 * time.Second)
			continue
		}

		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()

		if resp.StatusCode >= 200 && resp.StatusCode < 300 {
			slog.Info("health check passed", "service", name, "status", resp.StatusCode)
			return nil
		}

		lastErr = fmt.Errorf("attempt %d: HTTP %d: %s", attempt, resp.StatusCode, string(body))
		slog.Warn("health check unhealthy", "service", name, "attempt", attempt, "status", resp.StatusCode)
		time.Sleep(5 * time.Second)
	}

	return fmt.Errorf("health check failed for %s after %d retries: %w", name, retries, lastErr)
}
