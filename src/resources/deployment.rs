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
            TopologySpreadConstraint, Volume, VolumeMount,
        },
    },
    apimachinery::pkg::{
        api::resource::Quantity,
        apis::meta::v1::{LabelSelector, ObjectMeta},
        util::intstr::IntOrString,
    },
};
use kube::ResourceExt as _;

use super::labels::{descriptive_labels, infrastructure_annotations, owner_reference, standard_labels};
use crate::context::{ADMIN_PORT, praxis_image};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// UID the Praxis proxy container runs as (nobody/nfsnobody).
const PROXY_UID: i64 = 100;

/// Annotation overriding the data-plane replica count.
const REPLICAS_ANNOTATION: &str = "praxis.sh/replicas";

/// Replicas run when the Gateway does not ask for a specific count.
///
/// One, matching the behaviour every existing Gateway already has.
///
/// This was forced rather than chosen. Praxis 0.3.1 registered its KV
/// admin endpoints on the health port via `SO_REUSEPORT`, so probe
/// connections landed on whichever listener the kernel picked and
/// roughly half of them 404'd; every extra replica was another pod
/// whose liveness probe flapped, and a Gateway whose pods never settle
/// never reports Programmed.
///
/// The pinned data plane is now 0.5.2, which no longer registers that
/// endpoint, so the constraint is gone. Two is the better availability
/// default — a single-replica Gateway is a single point of failure for
/// every route attached to it — and raising it is now a live option
/// rather than a blocked one. It is deliberately not part of the
/// version bump: that is a behaviour change deserving its own
/// conformance run, and bundling it would make a red run ambiguous.
/// `praxis.sh/replicas` remains the per-Gateway override meanwhile.
const DEFAULT_REPLICAS: i32 = 1;

// -----------------------------------------------------------------------------
// Deployment Builder
// -----------------------------------------------------------------------------

/// Parameters for building a Praxis data-plane [`Deployment`].
pub struct DeploymentParams<'gw> {
    /// Child resource name.
    pub name: &'gw str,

    /// SHA-256 hex digest of the `ConfigMap` contents.
    ///
    /// Stored as a pod-template annotation so that config changes trigger a
    /// rolling restart. Required because Kubernetes `ConfigMap` volume mounts
    /// use atomic symlink swaps that `inotify`-based file watchers cannot
    /// detect.
    pub config_hash: &'gw str,

    /// Parent Gateway.
    pub gateway: &'gw Gateway,

    /// `(listener_name, port)` pairs from Gateway listeners.
    pub listener_ports: &'gw [(String, i32)],

    /// Target namespace.
    pub namespace: &'gw str,

    /// Deduplicated TLS secret names from HTTPS listeners.
    pub tls_secret_names: &'gw [String],
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
pub fn build_deployment(params: &DeploymentParams<'_>) -> crate::error::Result<Deployment> {
    let instance = params.gateway.name_any();
    let selector = standard_labels(&instance);

    // Everything the selector must not carry, because a selector cannot
    // be edited after creation and these can change with the Gateway.
    let mut labels = selector.clone();
    labels.extend(descriptive_labels(params.gateway));

    let mut pod_annotations = infrastructure_annotations(params.gateway);
    pod_annotations.insert("praxis.sh/config-hash".to_owned(), params.config_hash.to_owned());

    let (mut volume_mounts, mut volumes) = config_volume(params.name);
    let (tls_mounts, tls_vols) = build_tls_volumes(params.tls_secret_names);
    volume_mounts.extend(tls_mounts);
    volumes.extend(tls_vols);

    let (tmp_mount, tmp_vol) = tmp_volume();
    volume_mounts.push(tmp_mount);
    volumes.push(tmp_vol);

    let ports = build_container_ports(params.listener_ports);
    let container = build_praxis_container(ports, volume_mounts);

    let pod_template = build_pod_template(&labels, &selector, pod_annotations, container, volumes);

    build_deployment_object(
        params.name,
        params.namespace,
        params.gateway,
        DeploymentMetadata {
            labels,
            selector,
            pod_template,
        },
    )
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

    if listener_ports.iter().any(|(_, port)| *port == ADMIN_PORT) {
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
/// Wraps a single container in a hardened pod spec. `labels` go on the
/// pods; `selector` is the narrower, fixed set that identifies them.
fn build_pod_template(
    labels: &BTreeMap<String, String>,
    selector: &BTreeMap<String, String>,
    pod_annotations: BTreeMap<String, String>,
    container: Container,
    volumes: Vec<Volume>,
) -> PodTemplateSpec {
    let pod_spec = PodSpec {
        automount_service_account_token: Some(false),
        containers: vec![container],
        termination_grace_period_seconds: Some(15),
        topology_spread_constraints: Some(spread_constraints(selector)),
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

/// Returns the replica count for a Gateway's data plane.
///
/// Read from the `praxis.sh/replicas` annotation so an operator can size
/// a Gateway without a CRD of its own; the Gateway API's own extension
/// point, `spec.infrastructure.parametersRef`, is rejected by this
/// implementation. A malformed or non-positive value falls back to the
/// default rather than producing a Deployment that scales to zero.
fn desired_replicas(gateway: &Gateway) -> i32 {
    gateway
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(REPLICAS_ANNOTATION))
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|replicas| *replicas > 0)
        .unwrap_or(DEFAULT_REPLICAS)
}

/// Spreads data-plane pods across nodes.
///
/// Scheduling stays best-effort: on a single-node cluster a `DoNotSchedule`
/// constraint would leave every replica after the first pending forever.
fn spread_constraints(labels: &BTreeMap<String, String>) -> Vec<TopologySpreadConstraint> {
    vec![TopologySpreadConstraint {
        label_selector: Some(LabelSelector {
            match_labels: Some(labels.clone()),
            ..Default::default()
        }),
        max_skew: 1,
        topology_key: "kubernetes.io/hostname".to_owned(),
        when_unsatisfiable: "ScheduleAnyway".to_owned(),
        ..Default::default()
    }]
}

/// The label sets and pod template a [`Deployment`] is assembled from.
struct DeploymentMetadata {
    /// Labels stamped on the Deployment and its pods.
    labels: BTreeMap<String, String>,

    /// The immutable subset that selects those pods.
    selector: BTreeMap<String, String>,

    /// The pod template itself.
    pod_template: PodTemplateSpec,
}

/// Assembles the final [`Deployment`] object with metadata and spec.
///
/// Sets owner references, labels, rolling update strategy, and the pod
/// template.
fn build_deployment_object(
    name: &str,
    namespace: &str,
    gateway: &Gateway,
    meta: DeploymentMetadata,
) -> crate::error::Result<Deployment> {
    Ok(Deployment {
        metadata: ObjectMeta {
            annotations: Some(infrastructure_annotations(gateway)),
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            owner_references: Some(vec![owner_reference(gateway)?]),
            labels: Some(meta.labels),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(desired_replicas(gateway)),
            selector: LabelSelector {
                match_labels: Some(meta.selector),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some("RollingUpdate".to_owned()),
                ..Default::default()
            }),
            template: meta.pod_template,
            ..Default::default()
        }),
        ..Default::default()
    })
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::too_many_lines, clippy::default_trait_access, reason = "tests")]
mod tests {
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

    fn test_params<'gw>(gateway: &'gw Gateway, ports: &'gw [(String, i32)]) -> DeploymentParams<'gw> {
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
    fn test_deployment_selector_excludes_the_variable_labels() {
        let gateway = infrastructure_gateway();
        let deployment = build_deployment(&params(&gateway)).expect("a gateway with a uid builds");

        let spec = deployment.spec.expect("spec should be set");
        let selector = spec.selector.match_labels.expect("selector should be set");
        let pod_labels = spec
            .template
            .metadata
            .expect("template metadata should be set")
            .labels
            .expect("pod labels should be set");

        assert!(
            !selector.contains_key("key2") && !selector.contains_key(super::super::labels::GATEWAY_NAME_LABEL),
            "a Deployment selector cannot be edited after creation, so the first Gateway to \
             change spec.infrastructure would leave the operator unable to apply: {selector:?}"
        );
        assert!(
            selector.iter().all(|(key, value)| pod_labels.get(key) == Some(value)),
            "the selector still has to select the pods: {selector:?} vs {pod_labels:?}"
        );
    }

    #[test]
    fn test_pods_carry_gateway_infrastructure_metadata() {
        let gateway = infrastructure_gateway();
        let deployment = build_deployment(&params(&gateway)).expect("a gateway with a uid builds");

        let template = deployment
            .spec
            .expect("spec should be set")
            .template
            .metadata
            .expect("template metadata should be set");
        let labels = template.labels.expect("pod labels should be set");
        let annotations = template.annotations.expect("pod annotations should be set");

        assert_eq!(
            labels.get(super::super::labels::GATEWAY_NAME_LABEL),
            Some(&"test-gateway".to_owned()),
            "conformance finds an implementation's generated pods by this label"
        );
        assert_eq!(
            labels.get("key2"),
            Some(&"value2".to_owned()),
            "spec.infrastructure.labels asks for these on every generated resource"
        );
        assert_eq!(
            annotations.get("key1"),
            Some(&"value1".to_owned()),
            "spec.infrastructure.annotations likewise"
        );
        assert!(
            annotations.contains_key("praxis.sh/config-hash"),
            "the config hash still has to reach the pod template, or config edits stop rolling out"
        );
    }

    /// Builds a Gateway declaring infrastructure labels and annotations.
    fn infrastructure_gateway() -> Gateway {
        use gateway_api::gateways::GatewayInfrastructure;

        let mut gateway = test_gateway();
        gateway.spec.infrastructure = Some(GatewayInfrastructure {
            annotations: Some(BTreeMap::from([("key1".to_owned(), "value1".to_owned())])),
            labels: Some(BTreeMap::from([("key2".to_owned(), "value2".to_owned())])),
            ..Default::default()
        });
        gateway
    }

    /// Builds deployment params for a gateway with no listener ports.
    fn params(gateway: &Gateway) -> DeploymentParams<'_> {
        test_params(gateway, &[])
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
    fn test_replicas_honour_the_annotation() {
        let mut gateway = test_gateway();
        gateway.metadata.annotations = Some(BTreeMap::from([(REPLICAS_ANNOTATION.to_owned(), "5".to_owned())]));

        assert_eq!(
            desired_replicas(&gateway),
            5,
            "an explicit replica count should size the data plane"
        );
    }

    #[test]
    fn test_replicas_reject_nonsense_values() {
        for value in ["0", "-3", "many", ""] {
            let mut gateway = test_gateway();
            gateway.metadata.annotations = Some(BTreeMap::from([(REPLICAS_ANNOTATION.to_owned(), value.to_owned())]));

            assert_eq!(
                desired_replicas(&gateway),
                DEFAULT_REPLICAS,
                "a malformed replica annotation must not scale the data plane to zero: {value:?}"
            );
        }
    }

    #[test]
    fn test_pods_spread_across_nodes_without_blocking_scheduling() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let constraints = deployment
            .spec
            .and_then(|spec| spec.template.spec)
            .and_then(|pod| pod.topology_spread_constraints)
            .expect("spread constraints should be set");

        assert_eq!(
            constraints[0].topology_key, "kubernetes.io/hostname",
            "replicas should be spread across nodes"
        );
        assert_eq!(
            constraints[0].when_unsatisfiable, "ScheduleAnyway",
            "a single-node cluster must still schedule every replica"
        );
    }

    #[test]
    fn test_build_deployment_spec() {
        let gateway = test_gateway();
        let ports = vec![("http".to_owned(), 8080)];
        let deployment = build_deployment(&test_params(&gateway, &ports)).unwrap();

        let spec = deployment.spec.expect("spec should be set");
        assert_eq!(
            spec.replicas,
            Some(DEFAULT_REPLICAS),
            "a Gateway that asks for nothing should get the redundant default"
        );

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
