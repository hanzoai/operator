package controller

import (
	"context"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
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

// datastoreDefaults holds image and port defaults per DatastoreType.
type datastoreDefaults struct {
	image string
	port  int32
}

var defaultsByType = map[v1alpha1.DatastoreType]datastoreDefaults{
	v1alpha1.DatastoreTypePostgreSQL: {image: "ghcr.io/hanzoai/sql:18", port: 5432},
	v1alpha1.DatastoreTypeValkey:     {image: "hanzoai/kv:8", port: 6379},
	v1alpha1.DatastoreTypeDocDB:      {image: "ghcr.io/hanzoai/docdb:latest", port: 27017},
	v1alpha1.DatastoreTypeMinio:      {image: "ghcr.io/hanzoai/storage:latest", port: 9000},
	v1alpha1.DatastoreTypeNATS:       {image: "nats:2-alpine", port: 4222},
	v1alpha1.DatastoreTypeDatastore:  {image: "clickhouse/clickhouse-server:latest", port: 8123},
}

// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzodatastores,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzodatastores/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=hanzo.ai,resources=hanzodatastores/finalizers,verbs=update
// +kubebuilder:rbac:groups=apps,resources=statefulsets,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=persistentvolumeclaims,verbs=get;list;watch
// +kubebuilder:rbac:groups=batch,resources=cronjobs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=kms.hanzo.ai,resources=kmssecrets,verbs=get;list;watch;create;update;patch;delete

// HanzoDatastoreReconciler reconciles a HanzoDatastore object.
type HanzoDatastoreReconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Log    logr.Logger
}

// Reconcile implements the reconciliation loop for HanzoDatastore.
func (r *HanzoDatastoreReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	log := r.Log.WithValues("hanzodatastore", req.NamespacedName)

	// 1. Fetch the HanzoDatastore CR.
	ds := &v1alpha1.HanzoDatastore{}
	if err := r.Get(ctx, req.NamespacedName, ds); err != nil {
		if errors.IsNotFound(err) {
			log.Info("HanzoDatastore not found, skipping")
			return ctrl.Result{}, nil
		}
		return ctrl.Result{}, fmt.Errorf("fetching HanzoDatastore: %w", err)
	}

	// 2. Set phase to Creating if new.
	if ds.Status.Phase == "" || ds.Status.Phase == v1alpha1.PhasePending {
		status.SetDatastorePhase(&ds.Status, v1alpha1.PhaseCreating)
		status.SetCondition(&ds.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionTrue, "Reconciling", "Creating managed resources")
	}

	// Resolve defaults for this datastore type.
	defaults, ok := defaultsByType[ds.Spec.Type]
	if !ok {
		return ctrl.Result{}, fmt.Errorf("unsupported datastore type: %s", ds.Spec.Type)
	}

	name := ds.Name
	ns := ds.Namespace
	headlessSvcName := name + "-headless"

	// Labels.
	stdLabels := manifests.StandardLabels(name, string(ds.Spec.Type), ds.Spec.PartOf, "")
	selectorLabels := manifests.SelectorLabels(name)

	// Resolve image.
	containerImage := defaults.image
	if ds.Spec.Image != nil {
		containerImage = ds.Spec.Image.Repository
		if ds.Spec.Image.Tag != "" {
			containerImage = containerImage + ":" + ds.Spec.Image.Tag
		}
	}

	// Resolve port.
	containerPort := defaults.port
	if len(ds.Spec.Ports) > 0 {
		containerPort = ds.Spec.Ports[0].ContainerPort
	}

	// 3. Build containers.
	pullPolicy := corev1.PullIfNotPresent
	if ds.Spec.Image != nil && ds.Spec.Image.PullPolicy != "" {
		pullPolicy = ds.Spec.Image.PullPolicy
	}

	mainContainer := corev1.Container{
		Name:            name,
		Image:           containerImage,
		ImagePullPolicy: pullPolicy,
		Command:         ds.Spec.Command,
		Args:            ds.Spec.Args,
		Ports: []corev1.ContainerPort{
			{
				Name:          "data",
				ContainerPort: containerPort,
				Protocol:      corev1.ProtocolTCP,
			},
		},
		Env:     ds.Spec.Env,
		EnvFrom: ds.Spec.EnvFrom,
	}

	// Volume mount for data.
	mainContainer.VolumeMounts = append(mainContainer.VolumeMounts, corev1.VolumeMount{
		Name:      "data",
		MountPath: dataMountPath(ds.Spec.Type),
	})
	// Additional volume mounts from spec.
	mainContainer.VolumeMounts = append(mainContainer.VolumeMounts, ds.Spec.VolumeMounts...)

	if ds.Spec.Resources != nil {
		mainContainer.Resources = corev1.ResourceRequirements{
			Requests: ds.Spec.Resources.Requests,
			Limits:   ds.Spec.Resources.Limits,
		}
	}

	// Add a basic TCP readiness probe.
	mainContainer.ReadinessProbe = &corev1.Probe{
		ProbeHandler: corev1.ProbeHandler{
			TCPSocket: &corev1.TCPSocketAction{
				Port: intstr.FromInt32(containerPort),
			},
		},
		InitialDelaySeconds: 10,
		PeriodSeconds:       10,
	}

	containers := []corev1.Container{mainContainer}

	// User-defined sidecars.
	containers = append(containers, ds.Spec.Sidecars...)

	// PVC template.
	storageClassName := ds.Spec.Storage.StorageClassName
	pvcTemplates := []corev1.PersistentVolumeClaim{
		{
			ObjectMeta: metav1.ObjectMeta{
				Name:   "data",
				Labels: selectorLabels,
			},
			Spec: corev1.PersistentVolumeClaimSpec{
				AccessModes:      []corev1.PersistentVolumeAccessMode{corev1.ReadWriteOnce},
				StorageClassName: &storageClassName,
				Resources: corev1.VolumeResourceRequirements{
					Requests: corev1.ResourceList{
						corev1.ResourceStorage: ds.Spec.Storage.Size,
					},
				},
			},
		},
	}

	// Build StatefulSet.
	sts := manifests.BuildStatefulSet(
		name, ns,
		stdLabels, selectorLabels,
		ds.Spec.Replicas,
		containers,
		ds.Spec.Volumes,
		pvcTemplates,
		ds.Spec.ImagePullSecrets,
		headlessSvcName,
	)
	if err := r.createOrUpdate(ctx, ds, sts); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling StatefulSet: %w", err)
	}

	// 4. Build headless Service for StatefulSet DNS.
	svcPorts := []corev1.ServicePort{
		{
			Name:       "data",
			Port:       containerPort,
			TargetPort: intstr.FromInt32(containerPort),
			Protocol:   corev1.ProtocolTCP,
		},
	}
	headlessSvc := manifests.BuildHeadlessService(headlessSvcName, ns, stdLabels, svcPorts, selectorLabels)
	if err := r.createOrUpdate(ctx, ds, headlessSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling headless Service: %w", err)
	}

	// Also create a regular ClusterIP service for the primary name.
	regularSvc := manifests.BuildService(name, ns, stdLabels, svcPorts, selectorLabels)
	if err := r.createOrUpdate(ctx, ds, regularSvc); err != nil {
		return ctrl.Result{}, fmt.Errorf("reconciling Service: %w", err)
	}

	// 5. Alias Services.
	for _, alias := range ds.Spec.ServiceAliases {
		aliasSvc := manifests.BuildService(alias, ns, stdLabels, svcPorts, selectorLabels)
		if err := r.createOrUpdate(ctx, ds, aliasSvc); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling alias Service %s: %w", alias, err)
		}
	}

	// 6. Backup CronJob.
	if ds.Spec.Backup != nil && ds.Spec.Backup.Enabled {
		cronJob := r.buildBackupCronJob(ds, stdLabels)
		if err := r.createOrUpdate(ctx, ds, cronJob); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling backup CronJob: %w", err)
		}
	}

	// 7. KMSSecret CRs.
	for i := range ds.Spec.KMSSecrets {
		kmsRef := &ds.Spec.KMSSecrets[i]
		if err := r.reconcileKMSSecret(ctx, ds, kmsRef, stdLabels); err != nil {
			return ctrl.Result{}, fmt.Errorf("reconciling KMSSecret %s: %w", kmsRef.ManagedSecretName, err)
		}
	}

	// 8. Check readiness and update status.
	currentSTS := &appsv1.StatefulSet{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(sts), currentSTS); err != nil {
		return ctrl.Result{}, fmt.Errorf("fetching StatefulSet status: %w", err)
	}

	ds.Status.ReadyReplicas = currentSTS.Status.ReadyReplicas
	ds.Status.ObservedGeneration = ds.Generation

	desiredReplicas := int32(1)
	if ds.Spec.Replicas != nil {
		desiredReplicas = *ds.Spec.Replicas
	}

	// Connection string.
	ds.Status.ConnectionString = buildConnectionString(ds.Spec.Type, name, ns, containerPort)

	if currentSTS.Status.ReadyReplicas >= desiredReplicas && desiredReplicas > 0 {
		status.SetDatastorePhase(&ds.Status, v1alpha1.PhaseRunning)
		status.SetCondition(&ds.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionTrue, "Available", "All replicas are ready")
		status.SetCondition(&ds.Status.Conditions, v1alpha1.ConditionTypeProgressing,
			metav1.ConditionFalse, "Complete", "Reconciliation complete")
	} else if currentSTS.Status.ReadyReplicas > 0 {
		status.SetDatastorePhase(&ds.Status, v1alpha1.PhaseDegraded)
		status.SetCondition(&ds.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "Degraded",
			fmt.Sprintf("%d/%d replicas ready", currentSTS.Status.ReadyReplicas, desiredReplicas))
		status.SetCondition(&ds.Status.Conditions, v1alpha1.ConditionTypeDegraded,
			metav1.ConditionTrue, "InsufficientReplicas", "Not all replicas are ready")
	} else {
		status.SetDatastorePhase(&ds.Status, v1alpha1.PhaseCreating)
		status.SetCondition(&ds.Status.Conditions, v1alpha1.ConditionTypeReady,
			metav1.ConditionFalse, "NotReady", "No replicas are ready yet")
	}

	if err := r.Status().Update(ctx, ds); err != nil {
		return ctrl.Result{}, fmt.Errorf("updating HanzoDatastore status: %w", err)
	}

	log.Info("Reconciliation complete", "phase", ds.Status.Phase)
	return ctrl.Result{}, nil
}

// dataMountPath returns the data directory for each datastore type.
func dataMountPath(t v1alpha1.DatastoreType) string {
	switch t {
	case v1alpha1.DatastoreTypePostgreSQL:
		return "/var/lib/postgresql/data"
	case v1alpha1.DatastoreTypeValkey:
		return "/data"
	case v1alpha1.DatastoreTypeDocDB:
		return "/data/db"
	case v1alpha1.DatastoreTypeMinio:
		return "/data"
	case v1alpha1.DatastoreTypeNATS:
		return "/data/nats"
	case v1alpha1.DatastoreTypeDatastore:
		return "/var/lib/clickhouse"
	default:
		return "/data"
	}
}

// buildConnectionString generates an in-cluster connection string.
func buildConnectionString(t v1alpha1.DatastoreType, name, namespace string, port int32) string {
	host := fmt.Sprintf("%s.%s.svc.cluster.local", name, namespace)
	switch t {
	case v1alpha1.DatastoreTypePostgreSQL:
		return fmt.Sprintf("postgresql://%s:%d", host, port)
	case v1alpha1.DatastoreTypeValkey:
		return fmt.Sprintf("redis://%s:%d", host, port)
	case v1alpha1.DatastoreTypeDocDB:
		return fmt.Sprintf("mongodb://%s:%d", host, port)
	case v1alpha1.DatastoreTypeMinio:
		return fmt.Sprintf("http://%s:%d", host, port)
	case v1alpha1.DatastoreTypeNATS:
		return fmt.Sprintf("nats://%s:%d", host, port)
	case v1alpha1.DatastoreTypeDatastore:
		return fmt.Sprintf("http://%s:%d", host, port)
	default:
		return fmt.Sprintf("%s:%d", host, port)
	}
}

// buildBackupCronJob creates a CronJob for datastore backups.
func (r *HanzoDatastoreReconciler) buildBackupCronJob(
	ds *v1alpha1.HanzoDatastore,
	labels map[string]string,
) *batchv1.CronJob {
	name := ds.Name + "-backup"

	// Build the backup command based on datastore type.
	backupCmd := backupCommand(ds)

	env := []corev1.EnvVar{}
	if ds.Spec.Backup.S3Endpoint != "" {
		env = append(env,
			corev1.EnvVar{Name: "S3_ENDPOINT", Value: ds.Spec.Backup.S3Endpoint},
			corev1.EnvVar{Name: "S3_BUCKET", Value: ds.Spec.Backup.S3Bucket},
		)
	}

	var envFrom []corev1.EnvFromSource
	if ds.Spec.Backup.S3CredentialsSecret != "" {
		envFrom = append(envFrom, corev1.EnvFromSource{
			SecretRef: &corev1.SecretEnvSource{
				LocalObjectReference: corev1.LocalObjectReference{
					Name: ds.Spec.Backup.S3CredentialsSecret,
				},
			},
		})
	}

	successfulJobsHistoryLimit := int32(3)
	failedJobsHistoryLimit := int32(1)

	return &batchv1.CronJob{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: ds.Namespace,
			Labels:    labels,
		},
		Spec: batchv1.CronJobSpec{
			Schedule:                   ds.Spec.Backup.Schedule,
			SuccessfulJobsHistoryLimit: &successfulJobsHistoryLimit,
			FailedJobsHistoryLimit:     &failedJobsHistoryLimit,
			JobTemplate: batchv1.JobTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{
					Labels: labels,
				},
				Spec: batchv1.JobSpec{
					Template: corev1.PodTemplateSpec{
						ObjectMeta: metav1.ObjectMeta{
							Labels: labels,
						},
						Spec: corev1.PodSpec{
							RestartPolicy: corev1.RestartPolicyOnFailure,
							Containers: []corev1.Container{
								{
									Name:    "backup",
									Image:   "ghcr.io/hanzoai/backup:latest",
									Command: []string{"/bin/sh", "-c", backupCmd},
									Env:     env,
									EnvFrom: envFrom,
									Resources: corev1.ResourceRequirements{
										Requests: corev1.ResourceList{
											corev1.ResourceCPU:    resource.MustParse("100m"),
											corev1.ResourceMemory: resource.MustParse("128Mi"),
										},
										Limits: corev1.ResourceList{
											corev1.ResourceCPU:    resource.MustParse("500m"),
											corev1.ResourceMemory: resource.MustParse("512Mi"),
										},
									},
								},
							},
						},
					},
				},
			},
		},
	}
}

// backupCommand returns the backup shell command for a given datastore type.
func backupCommand(ds *v1alpha1.HanzoDatastore) string {
	host := fmt.Sprintf("%s.%s.svc.cluster.local", ds.Name, ds.Namespace)
	defaults := defaultsByType[ds.Spec.Type]

	switch ds.Spec.Type {
	case v1alpha1.DatastoreTypePostgreSQL:
		return fmt.Sprintf("pg_dumpall -h %s -p %d | gzip > /tmp/backup.sql.gz && backup-upload /tmp/backup.sql.gz", host, defaults.port)
	case v1alpha1.DatastoreTypeValkey:
		return fmt.Sprintf("redis-cli -h %s -p %d --rdb /tmp/dump.rdb && backup-upload /tmp/dump.rdb", host, defaults.port)
	case v1alpha1.DatastoreTypeDocDB:
		return fmt.Sprintf("mongodump --host=%s --port=%d --archive=/tmp/backup.archive && backup-upload /tmp/backup.archive", host, defaults.port)
	case v1alpha1.DatastoreTypeDatastore:
		return fmt.Sprintf("clickhouse-client --host=%s --port=%d -q 'SELECT 1' && clickhouse-backup create && backup-upload /tmp/backup", host, defaults.port)
	default:
		return "echo 'backup not supported for this datastore type'"
	}
}

// createOrUpdate creates or updates a resource with owner reference.
func (r *HanzoDatastoreReconciler) createOrUpdate(
	ctx context.Context,
	owner *v1alpha1.HanzoDatastore,
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

// reconcileKMSSecret creates or updates an unstructured KMSSecret CR.
func (r *HanzoDatastoreReconciler) reconcileKMSSecret(
	ctx context.Context,
	owner *v1alpha1.HanzoDatastore,
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

	result, err := ctrl.CreateOrUpdate(ctx, r.Client, existing, func() error {
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
	if err != nil {
		return err
	}

	if result != controllerutil.OperationResultNone {
		r.Log.Info("KMSSecret reconciled", "name", kms.GetName(), "result", result)
	}
	return nil
}

// SetupWithManager registers the HanzoDatastore controller with the manager.
func (r *HanzoDatastoreReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&v1alpha1.HanzoDatastore{}, builder.WithPredicates(createOrUpdatePred())).
		Owns(&appsv1.StatefulSet{}, builder.WithPredicates(
			predicate.Or(updateOrDeletePred(), statusChangePred()),
		)).
		Owns(&corev1.Service{}, builder.WithPredicates(updateOrDeletePred())).
		Owns(&batchv1.CronJob{}, builder.WithPredicates(updateOrDeletePred())).
		Named("hanzodatastore").
		Complete(r)
}
