# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM golang:1.26-alpine AS builder

ARG TARGETOS TARGETARCH

RUN apk add --no-cache git

WORKDIR /workspace

# Cache dependencies.
COPY go.mod go.sum ./
RUN --mount=type=cache,target=/go/pkg/mod \
    go mod download

# Build.
COPY . .
RUN --mount=type=cache,target=/go/pkg/mod \
    --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 GOOS=${TARGETOS} GOARCH=${TARGETARCH} go build \
    -ldflags="-w -s" \
    -o /hanzo-operator \
    cmd/main.go

# Runtime image.
FROM alpine:3.23

RUN apk add --no-cache ca-certificates tzdata && \
    adduser -u 65532 -S -D -H operator

COPY --from=builder /hanzo-operator /hanzo-operator

USER 65532

ENTRYPOINT ["/hanzo-operator"]
