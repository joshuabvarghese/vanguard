# Deploying Vanguard to Azure (AKS + Entra ID + Key Vault + Monitor)

This is the flagship deployment target — everything here is real, provisionable
infrastructure, not a narrated diagram. It provisions:

| Piece | Resource | Why |
|---|---|---|
| Compute | AKS, OIDC issuer + Workload Identity enabled | Runs the operator/API/reconcile loop; pods get Azure AD tokens with zero stored secrets |
| Registry | Azure Container Registry | Holds the `vanguard` image; AKS pulls via managed identity, no `imagePullSecret` |
| Identity | Entra ID (App Registration) + a user-assigned managed identity federated to AKS | Two different identity concerns — see below |
| Secrets | Key Vault (RBAC mode) + Secrets Store CSI driver | Control-plane's own secrets vs. tenant workload secrets — deliberately separate paths, see `src/cloud/azure/keyvault.rs` |
| Observability | Log Analytics + Application Insights | Cluster metrics (Container Insights) + application traces (OpenTelemetry export from Vanguard itself) |

## Two separate identity concerns — don't conflate them

1. **Workload Identity** (the user-assigned managed identity + federated
   credential in `main.bicep`) — lets the *vanguard pod itself* authenticate
   to Azure (Key Vault, Monitor) with no secret material anywhere. This is
   what `src/cloud/azure/keyvault.rs` and `monitor.rs` use via
   `DefaultAzureCredential`.
2. **Entra ID App Registration** (created once via the `az` commands below,
   not by Bicep) — authenticates *external callers of Vanguard's REST API*.
   This is what `src/cloud/azure/identity.rs`'s `EntraIdVerifier` validates
   tokens against. Bicep/ARM doesn't manage Graph app registrations well
   (no clean secret-rotation story from IaC), so this one step is manual/CLI.

## 1. Deploy the infrastructure

```bash
az group create -n vanguard-rg -l eastus2

az deployment group create \
  -g vanguard-rg \
  -f infra/bicep/main.bicep \
  -p infra/bicep/main.bicepparam

# capture outputs for later steps
AKS_NAME=$(az deployment group show -g vanguard-rg -n main --query properties.outputs.aksName.value -o tsv)
ACR_LOGIN_SERVER=$(az deployment group show -g vanguard-rg -n main --query properties.outputs.acrLoginServer.value -o tsv)
KV_URI=$(az deployment group show -g vanguard-rg -n main --query properties.outputs.keyVaultUri.value -o tsv)
WORKLOAD_CLIENT_ID=$(az deployment group show -g vanguard-rg -n main --query properties.outputs.workloadIdentityClientId.value -o tsv)
APPINSIGHTS_CONN=$(az deployment group show -g vanguard-rg -n main --query properties.outputs.appInsightsConnectionString.value -o tsv)
```

## 2. Create the Entra ID App Registration (REST API auth)

```bash
az ad app create --display-name "Vanguard Control Plane API" \
  --sign-in-audience AzureADMyOrg
APP_ID=$(az ad app list --display-name "Vanguard Control Plane API" --query "[0].appId" -o tsv)
TENANT_ID=$(az account show --query tenantId -o tsv)

# App role for callers allowed to mutate tenants (see api.rs::require_role)
az ad app update --id "$APP_ID" --app-roles '[{
  "allowedMemberTypes": ["User","Application"],
  "displayName": "Tenant Writer",
  "id": "'"$(uuidgen)"'",
  "isEnabled": true,
  "value": "Tenant.Write"
}]'
```

## 3. Push the image and grant AKS access to it

```bash
az acr build -r "${ACR_LOGIN_SERVER%%.*}" -t vanguard:latest .
```

(`az acr build` builds in ACR directly — no local Docker daemon needed, and
sidesteps the multi-stage `Dockerfile`'s build-time dependency on network
access this sandbox doesn't have for a from-scratch `docker build`.)

## 4. Wire the cluster

```bash
az aks get-credentials -g vanguard-rg -n "$AKS_NAME"

kubectl create namespace vanguard-system
kubectl apply -f manifests/crd/tenantpipeline.yaml
kubectl apply -f manifests/rbac/operator.yaml -n vanguard-system

# Annotate the ServiceAccount with the workload identity client ID
sed "s/<WORKLOAD_IDENTITY_CLIENT_ID>/$WORKLOAD_CLIENT_ID/" \
  manifests/azure/serviceaccount.yaml | kubectl apply -f -

# Fill in Key Vault name (Bicep only outputs the URI, so look up the name):
KV_NAME=$(az keyvault list -g vanguard-rg --query "[0].name" -o tsv)

sed "s/<WORKLOAD_IDENTITY_CLIENT_ID>/$WORKLOAD_CLIENT_ID/; s/<TENANT_ID>/$(az account show --query tenantId -o tsv)/; s/keyvaultName: \"\"/keyvaultName: \"$KV_NAME\"/" \
  manifests/azure/secretproviderclass.yaml | kubectl apply -n vanguard-system -f -

kubectl apply -f manifests/azure/deployment.yaml -n vanguard-system
```

`manifests/azure/deployment.yaml` sets `VANGUARD_AUTH_MODE=entra`,
`VANGUARD_ENTRA_TENANT_ID`/`VANGUARD_ENTRA_AUDIENCE`,
`VANGUARD_KEYVAULT_URL`, and `VANGUARD_APPINSIGHTS_CONNECTION_STRING` from
the values above, and the binary must be built with `cargo build --release
--features azure` — Rust 1.97 (this project's `rust-version` pin) is all
`--features azure` needs; no separate toolchain required.

## Cleanup

```bash
az group delete -n vanguard-rg --yes --no-wait
```
