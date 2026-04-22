//! Kubernetes workspace pod launcher.
//!
//! Creates and destroys workspace Pods, Services, and PVCs via the k8s API.
//! When not running in a k8s cluster, all operations are no-ops and the
//! existing manual-agent-connection behavior is preserved.

use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerPort, EnvVar, EnvVarSource, KeyToPath, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, Pod, PodSpec, ResourceRequirements,
    SecretKeySelector, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, PostParams};
use kube::Client;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum K8sError {
    #[error("Kubernetes client error: {0}")]
    Kube(#[from] kube::Error),
    #[error("Not running in Kubernetes")]
    NotInCluster,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    pub task_id: String,
    pub slug: String,
    pub owner: String,
    pub image: String,
    pub http_port: u16,
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub pod_name: String,
    pub service_name: String,
    pub pvc_name: String,
}

/// Kubernetes workspace manager. None when not running in a cluster.
#[derive(Clone)]
pub struct K8sClient {
    client: Client,
    namespace: String,
}

impl K8sClient {
    /// Try to create a client from in-cluster config. Returns None if not in k8s.
    pub async fn try_new() -> Option<Self> {
        if std::env::var("KUBERNETES_SERVICE_HOST").is_err() {
            tracing::info!("Not running in Kubernetes — k8s features disabled");
            return None;
        }
        match Client::try_default().await {
            Ok(client) => {
                let namespace = std::env::var("POD_NAMESPACE")
                    .unwrap_or_else(|_| "default".to_string());
                tracing::info!("Kubernetes client initialized (namespace: {})", namespace);
                Some(Self { client, namespace })
            }
            Err(e) => {
                tracing::warn!("Failed to create k8s client: {}", e);
                None
            }
        }
    }

    /// Generate the pod spec for a workspace (testable without a cluster).
    pub fn workspace_pod_spec(spec: &WorkspaceSpec, namespace: &str) -> Pod {
        let labels = BTreeMap::from([
            ("app".to_string(), "slopcoder-workspace".to_string()),
            ("slopcoder.dev/user".to_string(), spec.owner.clone()),
            ("slopcoder.dev/task-id".to_string(), spec.task_id.clone()),
        ]);

        Pod {
            metadata: ObjectMeta {
                name: Some(format!("workspace-{}", spec.slug)),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "workspace".to_string(),
                    image: Some(spec.image.clone()),
                    command: Some(vec!["/usr/local/bin/workspace-init".to_string()]),
                    env: Some(vec![
                        EnvVar {
                            name: "SLOPCODER_SERVER".to_string(),
                            value: Some(format!("ws://slopcoder-server.{}.svc:8080/agent/connect", namespace)),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "SLOPCODER_AGENT_PASSWORD".to_string(),
                            value_from: Some(EnvVarSource {
                                secret_key_ref: Some(SecretKeySelector {
                                    name: "slopcoder-internal".to_string(),
                                    key: "agent-password".to_string(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "GITHUB_USER".to_string(),
                            value: Some(spec.owner.clone()),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "WORKSPACE_SLUG".to_string(),
                            value: Some(spec.slug.clone()),
                            ..Default::default()
                        },
                    ]),
                    ports: Some(vec![
                        ContainerPort { container_port: 8080, name: Some("code-server".to_string()), ..Default::default() },
                        ContainerPort { container_port: 22, name: Some("ssh".to_string()), ..Default::default() },
                    ]),
                    volume_mounts: Some(vec![
                        VolumeMount {
                            name: "workspace-data".to_string(),
                            mount_path: "/home/dev/workspace".to_string(),
                            ..Default::default()
                        },
                        VolumeMount {
                            name: "ssh-keys".to_string(),
                            mount_path: "/home/dev/.ssh".to_string(),
                            read_only: Some(true),
                            ..Default::default()
                        },
                    ]),
                    resources: Some(ResourceRequirements {
                        requests: Some(BTreeMap::from([
                            ("cpu".to_string(), Quantity("1".to_string())),
                            ("memory".to_string(), Quantity("2Gi".to_string())),
                        ])),
                        limits: Some(BTreeMap::from([
                            ("cpu".to_string(), Quantity("4".to_string())),
                            ("memory".to_string(), Quantity("8Gi".to_string())),
                        ])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                volumes: Some(vec![
                    Volume {
                        name: "workspace-data".to_string(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: format!("workspace-{}", spec.slug),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    Volume {
                        name: "ssh-keys".to_string(),
                        config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
                            name: format!("ssh-keys-{}", spec.owner),
                            default_mode: Some(0o600),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub async fn create_workspace(&self, spec: &WorkspaceSpec) -> Result<WorkspaceInfo, K8sError> {
        let pod_name = format!("workspace-{}", spec.slug);
        let svc_name = format!("workspace-{}", spec.slug);
        let pvc_name = format!("workspace-{}", spec.slug);

        // Create PVC
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(pvc_name.clone()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([
                        ("storage".to_string(), Quantity("50Gi".to_string())),
                    ])),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = pvcs.create(&PostParams::default(), &pvc).await; // Ignore if exists

        // Create Pod
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = Self::workspace_pod_spec(spec, &self.namespace);
        pods.create(&PostParams::default(), &pod).await?;

        // Create Service
        let svcs: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(svc_name.clone()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([
                    ("slopcoder.dev/task-id".to_string(), spec.task_id.clone()),
                ])),
                ports: Some(vec![
                    ServicePort { port: 8080, name: Some("code-server".to_string()), ..Default::default() },
                    ServicePort { port: 22, name: Some("ssh".to_string()), ..Default::default() },
                    ServicePort { port: spec.http_port as i32, name: Some("http".to_string()), ..Default::default() },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        svcs.create(&PostParams::default(), &svc).await?;

        Ok(WorkspaceInfo { pod_name, service_name: svc_name, pvc_name })
    }

    pub async fn delete_workspace(&self, slug: &str) -> Result<(), K8sError> {
        let dp = DeleteParams::default();
        let name = format!("workspace-{}", slug);

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = pods.delete(&name, &dp).await;

        let svcs: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let _ = svcs.delete(&name, &dp).await;

        // PVC retained by default
        Ok(())
    }

    pub async fn create_ssh_keys_configmap(&self, username: &str, keys: &str) -> Result<(), K8sError> {
        let cms: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let cm = ConfigMap {
            metadata: ObjectMeta {
                name: Some(format!("ssh-keys-{}", username)),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([
                ("authorized_keys".to_string(), keys.to_string()),
            ])),
            ..Default::default()
        };
        let _ = cms.create(&PostParams::default(), &cm).await;
        Ok(())
    }
}

/// Fetch a user's SSH public keys from GitHub.
pub async fn fetch_github_ssh_keys(username: &str) -> Result<String, reqwest::Error> {
    let url = format!("https://github.com/{}.keys", username);
    reqwest::get(&url).await?.text().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_pod_spec_generation() {
        let spec = WorkspaceSpec {
            task_id: "abc-123".to_string(),
            slug: "fix-login".to_string(),
            owner: "alice".to_string(),
            image: "ghcr.io/org/slopcoder-agent:latest".to_string(),
            http_port: 3000,
        };
        let pod = K8sClient::workspace_pod_spec(&spec, "slopcoder");
        let meta = pod.metadata;
        assert_eq!(meta.name.unwrap(), "workspace-fix-login");
        assert_eq!(meta.namespace.unwrap(), "slopcoder");
        let labels = meta.labels.unwrap();
        assert_eq!(labels["slopcoder.dev/user"], "alice");
        assert_eq!(labels["slopcoder.dev/task-id"], "abc-123");

        let container = &pod.spec.unwrap().containers[0];
        assert_eq!(container.image.as_deref(), Some("ghcr.io/org/slopcoder-agent:latest"));
        assert_eq!(container.ports.as_ref().unwrap().len(), 2);
        assert_eq!(container.env.as_ref().unwrap().len(), 4);
    }

    #[test]
    fn test_workspace_pod_spec_env_vars() {
        let spec = WorkspaceSpec {
            task_id: "t1".to_string(),
            slug: "my-ws".to_string(),
            owner: "bob".to_string(),
            image: "img:latest".to_string(),
            http_port: 5173,
        };
        let pod = K8sClient::workspace_pod_spec(&spec, "ns");
        let envs = pod.spec.unwrap().containers[0].env.as_ref().unwrap().clone();
        let env_map: HashMap<String, Option<String>> = envs.into_iter().map(|e| (e.name, e.value)).collect();
        assert_eq!(env_map["GITHUB_USER"], Some("bob".to_string()));
        assert_eq!(env_map["WORKSPACE_SLUG"], Some("my-ws".to_string()));
        assert!(env_map["SLOPCODER_SERVER"].as_ref().unwrap().contains("ns.svc"));
    }
}
