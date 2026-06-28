---
title: "Operations & Security"
---

## Running the Controller

```bash
# Start the Rise backend (includes the Kubernetes deployment controller)
rise backend server
```

The controller will:
1. Connect to Kubernetes using configured kubeconfig or in-cluster credentials
2. Start the webhook server (sync and finalize endpoints) on a separate internal port
3. Metacontroller periodically calls these webhooks to reconcile each `RiseProject` resource, creating/updating child resources (namespaces, deployments, services, etc.) based on the desired state returned by the sync webhook

## Required RBAC Permissions

With Metacontroller, Rise itself only needs minimal permissions. Metacontroller handles the broad resource management (namespaces, deployments, services, secrets, ingresses, etc.) through its own RBAC.

Rise's ClusterRole:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: rise-controller
rules:
  # RiseProject CRD lifecycle management
  - apiGroups: ["rise.dev"]
    resources: ["riseprojects"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  # Pod read access for health checks in sync webhook and log streaming
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "watch"]
  # Pod logs for the log streaming endpoint
  - apiGroups: [""]
    resources: ["pods/log"]
    verbs: ["get"]
  # Events for monitoring pod errors in sync webhook
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["get", "list", "watch"]
  # EndpointSlices for backend service routing (applied directly via kube-rs)
  - apiGroups: ["discovery.k8s.io"]
    resources: ["endpointslices"]
    verbs: ["get", "patch"]
```

**Note:** Metacontroller itself needs broad permissions to manage child resources (namespaces, deployments, services, secrets, ingresses, etc.). Those are configured in the Metacontroller operator's own RBAC, not in Rise's ClusterRole.

## Troubleshooting

**Permission errors**:
```
Error: Forbidden (403): riseprojects.rise.dev is forbidden
```
- Verify Rise's service account has the required RBAC permissions (RiseProject CRD, pods, pod logs, events)
- Check `kubectl auth can-i` for each required verb/resource
- If child resources (deployments, services, etc.) fail to be created, check Metacontroller's RBAC permissions instead

**Connection errors**:
```
Error: Failed to connect to Kubernetes API
```
- Verify kubeconfig path is correct
- Check network connectivity to API server
- Ensure credentials are valid

**Image pull failures**:
```
Pod status: ImagePullBackOff
```
- Check secret exists: `kubectl get secret rise-registry-creds -n rise-{project}`
- Verify registry credentials are valid
- Check secret refresh logs in controller output
- Ensure image reference is correct

**Pods not becoming ready**:
- Check pod logs: `kubectl logs -n rise-{project} {pod-name}`
- Check pod events: `kubectl describe pod -n rise-{project} {pod-name}`
- Verify application listens on configured HTTP port
- Check resource limits and node capacity

## Webhook Security

The Metacontroller sync/finalize webhooks are served on a **separate internal port** (default: 3001). Authentication uses two independent layers — no shared secret is required.

### Defense-in-depth layers

1. **NetworkPolicy** — restricts ingress on the webhook port to pods labelled `app.kubernetes.io/name=metacontroller-operator`. External callers and wrong-namespace pods are blocked before they reach the Rise process.
2. **Pod-IP validation** — on every request, Rise checks that the TCP source IP belongs to a live metacontroller pod by querying the Kubernetes API (result cached for 15 seconds). If the Kubernetes API is unreachable, stale cache is used with a warning; if no cache exists yet, the request is rejected with `503`.

Together these layers mean an attacker must both bypass the NetworkPolicy *and* spoof the source IP of a live metacontroller pod — neither is possible without deep cluster compromise.

### In-transit confidentiality

The webhook speaks plain HTTP. The two layers above address *who can call* the webhook, but not *traffic sniffing* by a compromised pod with `NET_RAW` capability or a node-level attacker. The webhook response can include Kubernetes `Secret` resources (image pull credentials), so confidentiality of that traffic matters.

In most managed clusters (EKS, GKE, AKS) this is covered by node-level or CNI-level encryption. On bare-metal or on-prem clusters with an unencrypted CNI (e.g. plain VXLAN Flannel), you should either enable CNI encryption (WireGuard mode in Cilium/Calico, IPsec in Flannel) or deploy a service mesh with mTLS (Istio, Linkerd) to cover this gap.

A future alternative would be serving port 3001 over HTTPS directly, which would require a TLS certificate for the webhook service (e.g. issued by cert-manager) and the CA bundle injected into the `CompositeController` so Metacontroller can verify the server. This is not currently implemented.

### Configuration

When `metacontroller.enabled: true`, the Helm chart automatically injects the metacontroller pod namespace into the Rise config via the `RISE_METACONTROLLER_POD_NAMESPACE` environment variable. No manual configuration is needed.

If you supply the Rise config directly, set:

```yaml
deployment_controller:
  type: kubernetes
  metacontroller_webhook_port: 3001  # Default
  metacontroller_pod_namespace: "metacontroller"  # Namespace where metacontroller pods run
  # Optional — defaults to "app.kubernetes.io/name=metacontroller-operator"
  # metacontroller_pod_label_selector: "app.kubernetes.io/name=metacontroller-operator"
```

### Development mode

When `metacontroller_pod_namespace` is absent (or empty), pod-IP validation is skipped. Rise logs a startup warning and allows all webhook requests. This is intended for local development where Rise runs on the host outside the cluster. The server refuses to start without a namespace in any run mode other than `development`.

### Bring-your-own Metacontroller

When you manage the Metacontroller operator yourself (i.e. `metacontroller.install: false`), set `metacontroller.namespace` to the namespace where your operator runs so the NetworkPolicy and pod-IP validation both target the correct pods.

```yaml
# Helm values
metacontroller:
  enabled: true
  install: false                  # Do not install the operator subchart
  namespace: "my-metacontroller"  # Namespace where your operator runs
```
