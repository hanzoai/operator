package rollout

import (
	"bytes"
	"context"
	"fmt"
	"log/slog"
	"os/exec"
	"strings"
	"time"
)

// kubectl runs a kubectl command against a specific context and returns stdout.
func kubectl(ctx context.Context, kubeCtx string, args ...string) (string, error) {
	fullArgs := append([]string{"--context", kubeCtx}, args...)
	cmd := exec.CommandContext(ctx, "kubectl", fullArgs...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	slog.Debug("kubectl", "args", strings.Join(fullArgs, " "))
	if err := cmd.Run(); err != nil {
		return "", fmt.Errorf("kubectl %s: %s: %w", strings.Join(args, " "), strings.TrimSpace(stderr.String()), err)
	}
	return strings.TrimSpace(stdout.String()), nil
}

// GetCurrentImage returns the current image for a service in a cluster.
func GetCurrentImage(ctx context.Context, kubeCtx, namespace, kind, name string) (string, error) {
	out, err := kubectl(ctx, kubeCtx,
		"-n", namespace,
		"get", kind+"/"+name,
		"-o", "jsonpath={.spec.template.spec.containers[0].image}",
	)
	if err != nil {
		return "", err
	}
	if out == "" {
		return "", fmt.Errorf("service %s/%s not found in %s", namespace, name, kubeCtx)
	}
	return out, nil
}

// SetImage updates the image for a service and triggers a rollout.
func SetImage(ctx context.Context, kubeCtx, namespace, kind, name, image string) error {
	if _, err := kubectl(ctx, kubeCtx,
		"-n", namespace,
		"set", "image", kind+"/"+name, name+"="+image,
	); err != nil {
		return fmt.Errorf("set image %s: %w", name, err)
	}
	// Restart to ensure new pods pull the image
	if _, err := kubectl(ctx, kubeCtx,
		"-n", namespace,
		"rollout", "restart", kind+"/"+name,
	); err != nil {
		return fmt.Errorf("rollout restart %s: %w", name, err)
	}
	return nil
}

// WaitRollout waits for a rollout to complete within timeout.
func WaitRollout(ctx context.Context, kubeCtx, namespace, kind, name string, timeout time.Duration) error {
	timeoutStr := fmt.Sprintf("--timeout=%ds", int(timeout.Seconds()))
	_, err := kubectl(ctx, kubeCtx,
		"-n", namespace,
		"rollout", "status", kind+"/"+name, timeoutStr,
	)
	return err
}

// PortForward opens a kubectl port-forward to a pod backing the given service
// and returns the local address to dial. Callers must invoke cancel to release
// the forward.
func PortForward(ctx context.Context, kubeCtx, namespace, kind, name string, remotePort int) (localAddr string, cancel context.CancelFunc, err error) {
	pfCtx, pfCancel := context.WithCancel(ctx)

	// Find a pod for this service
	selector := fmt.Sprintf("app=%s", name)
	podName, err := kubectl(ctx, kubeCtx,
		"-n", namespace,
		"get", "pod",
		"-l", selector,
		"-o", "jsonpath={.items[0].metadata.name}",
	)
	if err != nil || podName == "" {
		// Fallback: try app.kubernetes.io/name
		podName, err = kubectl(ctx, kubeCtx,
			"-n", namespace,
			"get", "pod",
			"-l", fmt.Sprintf("app.kubernetes.io/name=%s", name),
			"-o", "jsonpath={.items[0].metadata.name}",
		)
		if err != nil || podName == "" {
			pfCancel()
			return "", nil, fmt.Errorf("no pods found for %s in %s/%s", name, kubeCtx, namespace)
		}
	}

	// Use port 0 so kubectl picks a free local port
	cmd := exec.CommandContext(pfCtx, "kubectl",
		"--context", kubeCtx,
		"-n", namespace,
		"port-forward", podName,
		fmt.Sprintf("::%d", remotePort),
	)
	var stderr bytes.Buffer
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Start(); err != nil {
		pfCancel()
		return "", nil, fmt.Errorf("port-forward start: %w", err)
	}

	// Wait for the "Forwarding from 127.0.0.1:XXXXX -> YYYYY" line
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		time.Sleep(200 * time.Millisecond)
		out := stdout.String() + stderr.String()
		if idx := strings.Index(out, "127.0.0.1:"); idx >= 0 {
			// Extract local port
			rest := out[idx+len("127.0.0.1:"):]
			end := strings.IndexAny(rest, " \n\t->")
			if end < 0 {
				end = len(rest)
			}
			localPort := rest[:end]
			localAddr = "http://127.0.0.1:" + localPort
			return localAddr, func() {
				pfCancel()
				_ = cmd.Wait()
			}, nil
		}
	}

	pfCancel()
	_ = cmd.Wait()
	return "", nil, fmt.Errorf("port-forward timeout for %s: %s", podName, stderr.String())
}
