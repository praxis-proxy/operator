// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Deployment builder for Praxis data-plane.

use std::collections::BTreeMap;

use gateway_api::gateways::Gateway;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
        core::v1::{
            Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, HTTPGetAction,
            PodSpec, PodTemplateSpec, Probe, ResourceRequirements, SeccompProfile, SecretVolumeSource, SecurityContext,
            Volume, VolumeMount,
        },
    },
    apimachinery::pkg::{
        api::resource::Quantity,
        apis::meta::v1::{LabelSelector, ObjectMeta},
        util::intstr::IntOrString,
    },
};
use kube::ResourceExt;

use super::labels::{owner_reference, standard_labels};
use crate::context::{ADMIN_PORT, praxis_image};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// UID the Praxis proxy container runs as (nobody/nfsnobody).
const PROXY_UID: i64 = 100;

// -----------------------------------------------------------------------------
// Deployment Builder
// -----------------------------------------------------------------------------

/// Parameters for building a Praxis data-plane [`Deployment`].
pub(crate) struct DeploymentParams<'a> {
    /// SHA-256 hex digest of the `ConfigMap` contents.
    ///
    /// Stored as a pod-template annotation so that config changes trigger a
    /// rolling restart. Required because Kubernetes `ConfigMap` volume mounts
    /// use atomic symlink swaps that `inotify`-based file watchers cannot
    /// detect.
    pub(crate) config_hash: &'a str,

    /// Child resource name.
    pub(crate) name: &'a str,

    /// Target namespace.
    pub(crate) namespace: &'a str,

    /// Parent Gateway.
    pub(crate) gateway: &'a Gateway,

    /// Deduplicated TLS secret names from HTTPS listeners.
    pub(crate) tls_secret_names: &'a [String],

    /// `(listener_name, port)` pairs from Gateway listeners.
    pub(crate) listener_ports: &'a [(String, i32)],
}

/// Builds a Deployment for the Praxis data-plane.
///
/// Creates a Deployment with a single replica running the Praxis proxy
/// container. The pod mounts the configuration `ConfigMap` and TLS secrets.
/// Health probes use the admin endpoint on [`ADMIN_PORT`]. Config updates
/// are picked up via the proxy's file watcher (no pod restart needed).
///
/// The `listener_ports` field provides `(name, port)` pairs from `Gateway`
/// listeners; the admin port is appended unless a listener already occupies
/// it.
///
/// [`ADMIN_PORT`]: crate::context::ADMIN_PORT
///
/// # Errors
///
/// Returns an error if the Gateway has no UID.
pub(crate) fn build_deployment(params: &DeploymentParams<'_>) -> crate::error::Result<Deployment> {
    let instance = params.gateway.name_any();
    let labels = standard_labels(&instance);
    let pod_annotations = BTreeMap::from([("praxis.sh/config-hash".to_owned(), params.config_hash.to_owned())]);

    let (mut volume_mounts, mut volumes) = config_volume(params.name);
    let (tls_mounts, tls_vols) = build_tls_volumes(params.tls_secret_names);
    volume_mounts.extend(tls_mounts);
    volumes.extend(tls_vols);

    let (tmp_mount, tmp_vol) = tmp_volume();
    volume_mounts.push(tmp_mount);
    volumes.push(tmp_vol);

    let ports = build_container_ports(params.listener_ports);
    let container = build_praxis_container(ports, volume_mounts);

    let pod_template = build_pod_template(&labels, pod_annotations, container, volumes);

    build_deployment_object(params.name, params.namespace, params.gateway, labels, pod_template)
}

// -----------------------------------------------------------------------------
// Volume Builders
// -----------------------------------------------------------------------------

/// Creates the base config volume and mount pair.
///
/// Returns `(volume_mounts, volumes)` for the `ConfigMap` mount.
fn config_volume(config_name: &str) -> (Vec<VolumeMount>, Vec<Volume>) {
    let mount = VolumeMount {
        name: "config".to_owned(),
        mount_path: "/etc/praxis".to_owned(),
        read_only: Some(true),
        ..Default::default()
    };
    let vol = Volume {
        name: "config".to_owned(),
        config_map: Some(ConfigMapVolumeSource {
            name: config_name.to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    (vec![mount], vec![vol])
}

/// Creates TLS secret volumes and mounts for each certificate.
///
/// Returns `(volume_mounts, volumes)` for all TLS secrets.
fn build_tls_volumes(tls_secret_names: &[String]) -> (Vec<VolumeMount>, Vec<Volume>) {
    let mut mounts = Vec::with_capacity(tls_secret_names.len());
    let mut volumes = Vec::with_capacity(tls_secret_names.len());

    for (i, secret_name) in tls_secret_names.iter().enumerate() {
        let vol_name = format!("tls-{i}");

        mounts.push(VolumeMount {
            name: vol_name.clone(),
            mount_path: format!("/tls/{secret_name}"),
            read_only: Some(true),
            ..Default::default()
        });

        volumes.push(Volume {
            name: vol_name,
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    (mounts, volumes)
}

/// Creates the writable `/tmp` volume and mount pair.
///
/// Required because the container uses a read-only root filesystem but
/// the proxy needs a writable temporary directory.
fn tmp_volume() -> (VolumeMount, Volume) {
    let mount = VolumeMount {
        name: "tmp".to_owned(),
        mount_path: "/tmp".to_owned(),
        ..Default::default()
    };
    let vol = Volume {
        name: "tmp".to_owned(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    };
    (mount, vol)
}

// -----------------------------------------------------------------------------
// Container Builders
// -----------------------------------------------------------------------------

/// Assembles container ports from listener ports, appending the admin port.
///
/// Warns and skips the admin port when a listener already occupies it.
fn build_container_ports(listener_ports: &[(String, i32)]) -> Vec<ContainerPort> {
    let mut ports: Vec<ContainerPort> = listener_ports
        .iter()
        .map(|(port_name, port_num)| ContainerPort {
            name: Some(port_name.clone()),
            container_port: *port_num,
            protocol: Some("TCP".to_owned()),
            ..Default::default()
        })
        .collect();

    if listener_ports.iter().any(|(_, p)| *p == ADMIN_PORT) {
        tracing::warn!(
            port = ADMIN_PORT,
            "listener port collides with admin port; skipping dedicated admin port"
        );
    } else {
        ports.push(ContainerPort {
            name: Some("admin".to_owned()),
            container_port: ADMIN_PORT,
            protocol: Some("TCP".to_owned()),
            ..Default::default()
        });
    }

    ports
}

/// Builds the Praxis proxy container spec with probes and security context.
///
/// Uses the image from [`praxis_image`] and hardened security defaults.
///
/// [`praxis_image`]: crate::context::praxis_image
fn build_praxis_container(ports: Vec<ContainerPort>, volume_mounts: Vec<VolumeMount>) -> Container {
    let resource_requests = BTreeMap::from([
        ("cpu".to_owned(), Quantity("100m".to_owned())),
        ("memory".to_owned(), Quantity("64Mi".to_owned())),
    ]);
    let resource_limits = BTreeMap::from([("memory".to_owned(), Quantity("256Mi".to_owned()))]);

    Container {
        name: "praxis".to_owned(),
        image: Some(praxis_image()),
        command: Some(vec!["praxis".to_owned()]),
        args: Some(vec!["--config".to_owned(), "/etc/praxis/config.yaml".to_owned()]),
        ports: Some(ports),
        volume_mounts: Some(volume_mounts),
        resources: Some(ResourceRequirements {
            limits: Some(resource_limits),
            requests: Some(resource_requests),
            ..Default::default()
        }),
        liveness_probe: Some(admin_probe("/healthy", 5, 3, 5)),
        readiness_probe: Some(admin_probe("/ready", 3, 2, 3)),
        startup_probe: Some(admin_probe("/healthy", 1, 30, 0)),
        security_context: Some(proxy_security_context()),
        ..Default::default()
    }
}

/// Creates an HTTP health probe against the admin port.
fn admin_probe(path: &str, period_seconds: i32, failure_threshold: i32, initial_delay: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_owned()),
            port: IntOrString::Int(ADMIN_PORT),
            ..Default::default()
        }),
        period_seconds: Some(period_seconds),
        failure_threshold: Some(failure_threshold),
        initial_delay_seconds: Some(initial_delay),
        ..Default::default()
    }
}

/// Returns the hardened security context for the proxy container.
///
/// Runs as non-root with a read-only filesystem, drops all capabilities
/// except `NET_BIND_SERVICE`, and uses the `RuntimeDefault` seccomp profile.
fn proxy_security_context() -> SecurityContext {
    SecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(PROXY_UID),
        read_only_root_filesystem: Some(true),
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            add: Some(vec!["NET_BIND_SERVICE".to_owned()]),
            drop: Some(vec!["ALL".to_owned()]),
        }),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_owned(),
            localhost_profile: None,
        }),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// Pod and Deployment Assembly
// -----------------------------------------------------------------------------

/// Builds the pod template spec with labels, annotations, and volumes.
///
/// Wraps a single container in a hardened pod spec.
fn build_pod_template(
    labels: &BTreeMap<String, String>,
    pod_annotations: BTreeMap<String, String>,
    container: Container,
    volumes: Vec<Volume>,
) -> PodTemplateSpec {
    let pod_spec = PodSpec {
        automount_service_account_token: Some(false),
        containers: vec![container],
        termination_grace_period_seconds: Some(15),
        volumes: Some(volumes),
        ..Default::default()
    };

    PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            annotations: Some(pod_annotations),
            ..Default::default()
        }),
        spec: Some(pod_spec),
    }
}

/// Assembles the final [`Deployment`] object with metadata and spec.
///
/// Sets owner references, labels, rolling update strategy, and the pod
/// template.
fn build_deployment_object(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
    labels: BTreeMap<String, String>,
    pod_template: PodTemplateSpec,
) -> crate::error::Result<Deployment> {
    Ok(Deployment {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(labels),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some("RollingUpdate".to_owned()),
                ..Default::default()
            }),
            template: pod_template,
            ..Default::default()
        }),
        ..Default::default()
    })
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::default_trait_access,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_assert_message,
    reason = "tests"
)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    fn test_gateway() -> Gateway {
        Gateway {
            metadata: ObjectMeta {
                name: Some("test-gateway".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some("test-uid".to_owned()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        }
    }

    fn test_params<'a>(gateway: &'a Gateway, ports: &'a [(String, i32)]) -> DeploymentParams<'a> {
        DeploymentParams {
            config_hash: "abc123",
            name: "praxis-deploy",
            namespace: "default",
            gateway,
            tls_secret_names: &[],
            listener_ports: ports,
        }
    }

    #[test]
    fn test_build_deployment_metadata() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        assert_eq!(
            deployment.metadata.name,
            Some("praxis-deploy".to_owned()),
            "name should match"
        );
        assert_eq!(
            deployment.metadata.namespace,
            Some("default".to_owned()),
            "namespace should match"
        );

        let labels = deployment.metadata.labels.expect("labels should be set");
        assert_eq!(
            labels.get("app.kubernetes.io/name"),
            Some(&"praxis".to_owned()),
            "app name label should be set"
        );
        assert_eq!(
            labels.get("app.kubernetes.io/instance"),
            Some(&"test-gateway".to_owned()),
            "instance label should match gateway name"
        );

        let owner_refs = deployment
            .metadata
            .owner_references
            .expect("owner references should be set");
        assert_eq!(owner_refs.len(), 1, "should have one owner reference");
        assert_eq!(owner_refs[0].kind, "Gateway", "owner kind should be Gateway");
    }

    #[test]
    fn test_build_deployment_spec() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        assert_eq!(spec.replicas, Some(1), "replicas should be 1");

        let selector = spec.selector;
        let match_labels = selector.match_labels.expect("match_labels should be set");
        assert_eq!(
            match_labels.get("app.kubernetes.io/instance"),
            Some(&"test-gateway".to_owned()),
            "selector should match instance"
        );
    }

    #[test]
    fn test_build_deployment_strategy() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let strategy = spec.strategy.expect("strategy should be set");
        assert_eq!(
            strategy.type_,
            Some("RollingUpdate".to_owned()),
            "strategy should be RollingUpdate to avoid traffic blackouts"
        );
    }

    #[test]
    fn test_build_deployment_pod_template() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        assert_eq!(pod_spec.containers.len(), 1, "should have one container");

        let container = &pod_spec.containers[0];
        assert_eq!(container.name, "praxis", "container name should be praxis");
        assert_eq!(
            container.image,
            Some(praxis_image()),
            "container image should match default"
        );
    }

    #[test]
    fn test_build_deployment_container_ports() {
        let gateway = test_gateway();
        let deployment = build_deployment(&DeploymentParams {
            listener_ports: &[("http".to_owned(), 80), ("https".to_owned(), 443)],
            ..test_params(&gateway, &[])
        })
        .unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let ports = container.ports.as_ref().expect("ports should be set");

        assert_eq!(ports.len(), 3, "should have listener ports + admin");
        assert_eq!(ports[0].name, Some("http".to_owned()), "first port name should be http");
        assert_eq!(ports[0].container_port, 80, "first port should be 80");
        assert_eq!(
            ports[1].name,
            Some("https".to_owned()),
            "second port name should be https"
        );
        assert_eq!(ports[1].container_port, 443, "second port should be 443");
        assert_eq!(
            ports[2].name,
            Some("admin".to_owned()),
            "last port name should be admin"
        );
        assert_eq!(ports[2].container_port, 9901, "admin port should be 9901");
    }

    #[test]
    fn test_build_deployment_volume_mounts() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let volume_mounts = container.volume_mounts.as_ref().expect("volume mounts should be set");

        assert_eq!(
            volume_mounts.len(),
            2,
            "should have config and tmp volume mounts without TLS"
        );
        assert_eq!(volume_mounts[0].name, "config", "first mount should be config");
        assert_eq!(
            volume_mounts[0].mount_path, "/etc/praxis",
            "config should mount to /etc/praxis"
        );
        assert_eq!(
            volume_mounts[0].read_only,
            Some(true),
            "config mount should be read-only"
        );
        assert_eq!(volume_mounts[1].name, "tmp", "second mount should be tmp");
        assert_eq!(volume_mounts[1].mount_path, "/tmp", "tmp should mount to /tmp");

        let volumes = pod_spec.volumes.as_ref().expect("volumes should be set");
        assert_eq!(volumes.len(), 2, "should have config and tmp volumes without TLS");
        assert_eq!(volumes[0].name, "config", "first volume should be config");
        assert!(volumes[0].config_map.is_some(), "config volume should be a ConfigMap");
        assert_eq!(volumes[1].name, "tmp", "second volume should be tmp");
        assert!(volumes[1].empty_dir.is_some(), "tmp volume should be emptyDir");
    }

    #[test]
    fn test_build_deployment_tmp_volume() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let volume_mounts = container.volume_mounts.as_ref().expect("volume mounts should be set");

        let tmp_mount = volume_mounts
            .iter()
            .find(|m| m.name == "tmp")
            .expect("tmp mount should exist");
        assert_eq!(tmp_mount.mount_path, "/tmp", "tmp should mount to /tmp");
        assert!(
            tmp_mount.read_only.is_none() || tmp_mount.read_only == Some(false),
            "tmp mount should be writable"
        );

        let volumes = pod_spec.volumes.as_ref().expect("volumes should be set");
        let tmp_vol = volumes
            .iter()
            .find(|v| v.name == "tmp")
            .expect("tmp volume should exist");
        assert!(tmp_vol.empty_dir.is_some(), "tmp volume should be emptyDir");
    }

    #[test]
    fn test_build_deployment_tls_volumes() {
        let gateway = test_gateway();
        let tls_secrets = vec!["my-cert".to_owned(), "other-cert".to_owned()];
        let deployment = build_deployment(&DeploymentParams {
            tls_secret_names: &tls_secrets,
            listener_ports: &[("https".to_owned(), 443)],
            ..test_params(&gateway, &[])
        })
        .unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let volume_mounts = container.volume_mounts.as_ref().expect("volume mounts should be set");

        assert_eq!(volume_mounts.len(), 4, "should have config + two TLS + tmp mounts");
        assert_eq!(
            volume_mounts[1].name, "tls-0",
            "first TLS mount name should be index-based"
        );
        assert_eq!(
            volume_mounts[1].mount_path, "/tls/my-cert",
            "first TLS mount path should use secret name"
        );
        assert_eq!(
            volume_mounts[2].name, "tls-1",
            "second TLS mount name should be index-based"
        );
        assert_eq!(
            volume_mounts[2].mount_path, "/tls/other-cert",
            "second TLS mount path should use secret name"
        );
        assert_eq!(volume_mounts[3].name, "tmp", "last mount should be tmp");

        let volumes = pod_spec.volumes.as_ref().expect("volumes should be set");
        assert_eq!(volumes.len(), 4, "should have config + two TLS + tmp volumes");
        assert_eq!(volumes[1].name, "tls-0", "first TLS volume name should be index-based");
        assert!(volumes[1].secret.is_some(), "first TLS volume should be a Secret");
        assert_eq!(
            volumes[1].secret.as_ref().unwrap().secret_name,
            Some("my-cert".to_owned()),
            "first TLS secret name should match"
        );
        assert_eq!(volumes[2].name, "tls-1", "second TLS volume name should be index-based");
        assert!(volumes[2].secret.is_some(), "second TLS volume should be a Secret");
        assert_eq!(volumes[3].name, "tmp", "last volume should be tmp");
        assert!(volumes[3].empty_dir.is_some(), "tmp volume should be emptyDir");
    }

    #[test]
    fn test_build_deployment_probes() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];

        let liveness = container.liveness_probe.as_ref().expect("liveness probe should be set");
        let liveness_http = liveness.http_get.as_ref().expect("liveness should use HTTP GET");
        assert_eq!(
            liveness_http.path,
            Some("/healthy".to_owned()),
            "liveness path should be /healthy"
        );
        assert_eq!(
            liveness_http.port,
            IntOrString::Int(9901),
            "liveness port should be 9901"
        );

        let readiness = container
            .readiness_probe
            .as_ref()
            .expect("readiness probe should be set");
        let readiness_http = readiness.http_get.as_ref().expect("readiness should use HTTP GET");
        assert_eq!(
            readiness_http.path,
            Some("/ready".to_owned()),
            "readiness path should be /ready"
        );
        assert_eq!(
            readiness_http.port,
            IntOrString::Int(9901),
            "readiness port should be 9901"
        );
    }

    #[test]
    fn test_build_deployment_security_context() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let security_context = container
            .security_context
            .as_ref()
            .expect("security context should be set");

        assert_eq!(
            security_context.run_as_non_root,
            Some(true),
            "run_as_non_root should be true"
        );
        assert_eq!(
            security_context.run_as_user,
            Some(PROXY_UID),
            "run_as_user should match PROXY_UID constant"
        );
        assert_eq!(
            security_context.read_only_root_filesystem,
            Some(true),
            "read_only_root_filesystem should be true"
        );
        assert_eq!(
            security_context.allow_privilege_escalation,
            Some(false),
            "allow_privilege_escalation should be false"
        );

        let seccomp = security_context
            .seccomp_profile
            .as_ref()
            .expect("seccomp profile should be set");
        assert_eq!(seccomp.type_, "RuntimeDefault", "seccomp type should be RuntimeDefault");
        assert_eq!(seccomp.localhost_profile, None, "localhost_profile should be None");
    }

    #[test]
    fn test_build_deployment_pod_hardening() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");

        assert_eq!(pod_spec.service_account_name, None, "service account should not be set");
        assert_eq!(
            pod_spec.automount_service_account_token,
            Some(false),
            "automount_service_account_token should be false"
        );
        assert_eq!(
            pod_spec.termination_grace_period_seconds,
            Some(15),
            "termination grace period should be 15"
        );
    }

    #[test]
    fn test_build_deployment_resource_requirements() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let resources = container.resources.as_ref().expect("resources should be set");

        let requests = resources.requests.as_ref().expect("requests should be set");
        assert_eq!(
            requests.get("cpu"),
            Some(&Quantity("100m".to_owned())),
            "cpu request should be 100m"
        );
        assert_eq!(
            requests.get("memory"),
            Some(&Quantity("64Mi".to_owned())),
            "memory request should be 64Mi"
        );

        let limits = resources.limits.as_ref().expect("limits should be set");
        assert_eq!(
            limits.get("memory"),
            Some(&Quantity("256Mi".to_owned())),
            "memory limit should be 256Mi"
        );
        assert!(limits.get("cpu").is_none(), "cpu limit should not be set");
    }

    #[test]
    fn test_build_deployment_admin_port_collision() {
        let gateway = test_gateway();
        let deployment = build_deployment(&DeploymentParams {
            listener_ports: &[("admin-listener".to_owned(), ADMIN_PORT)],
            ..test_params(&gateway, &[])
        })
        .unwrap();

        let spec = deployment.spec.expect("spec should be set");
        let pod_spec = spec.template.spec.expect("pod spec should be set");
        let container = &pod_spec.containers[0];
        let ports = container.ports.as_ref().expect("ports should be set");

        assert_eq!(
            ports.len(),
            1,
            "should only have listener port when it collides with admin port"
        );
        assert_eq!(
            ports[0].container_port, ADMIN_PORT,
            "single port should be the listener at admin port"
        );
    }
}
