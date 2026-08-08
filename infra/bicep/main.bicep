// Vanguard — Azure-native flagship deployment.
//
// Provisions:
//   - AKS cluster with the OIDC issuer + Workload Identity add-ons enabled
//     (no cluster secrets, no kubeconfig-embedded credentials — pods
//     authenticate to Azure via short-lived federated tokens)
//   - Azure Container Registry, with AcrPull granted to the cluster's
//     kubelet identity (so AKS can pull the vanguard image without an
//     imagePullSecret)
//   - Key Vault (RBAC authorization mode, not access policies — consistent
//     with the rest of this deployment's "no standing secrets" posture)
//   - A user-assigned managed identity for the vanguard workload, federated
//     to the AKS OIDC issuer for the `vanguard` ServiceAccount in the
//     `vanguard-system` namespace (see manifests/azure/serviceaccount.yaml)
//   - Log Analytics workspace + workspace-based Application Insights,
//     wired to AKS via Container Insights (`omsagent`) for cluster/pod
//     metrics and to Vanguard's own tracing export for app-level spans
//
// Entra ID App Registration (for REST API auth) is deliberately NOT
// created here — Bicep/ARM has no first-class resource type for Graph
// app registrations, and provisioning one from IaC without also managing
// its client secret rotation is a worse trade than a one-time `az ad
// app create` documented in infra/bicep/README.md.
//
// Deploy: az deployment group create -g <rg> -f main.bicep -p @main.bicepparam

targetScope = 'resourceGroup'

@description('Base name used to derive all resource names (must be globally unique for ACR/Key Vault).')
param namePrefix string = 'vanguard'

@description('Azure region for all resources.')
param location string = resourceGroup().location

@description('Kubernetes namespace the vanguard control-plane pod runs in.')
param vanguardNamespace string = 'vanguard-system'

@description('Kubernetes ServiceAccount name used by the vanguard pod (must match manifests/rbac/operator.yaml / manifests/azure/serviceaccount.yaml).')
param vanguardServiceAccount string = 'vanguard-operator'

@description('AKS node VM size.')
param nodeVmSize string = 'Standard_D2s_v5'

@description('AKS node count (single node pool — this is a control-plane demo cluster, not a production topology).')
param nodeCount int = 3

@description('Application Insights / Log Analytics retention in days.')
param logRetentionDays int = 30

var uniqueSuffix = uniqueString(resourceGroup().id)
var acrName = replace('${namePrefix}acr${uniqueSuffix}', '-', '')
var kvName = take('${namePrefix}-kv-${uniqueSuffix}', 24)
var aksName = '${namePrefix}-aks'
var uamiName = '${namePrefix}-workload-identity'
var lawName = '${namePrefix}-logs'
var appInsightsName = '${namePrefix}-insights'

// ─── Log Analytics + Application Insights (Azure Monitor) ─────────────────

resource logAnalytics 'Microsoft.OperationalInsights/workspaces@2023-09-01' = {
  name: lawName
  location: location
  properties: {
    sku: { name: 'PerGB2018' }
    retentionInDays: logRetentionDays
  }
}

resource appInsights 'Microsoft.Insights/components@2020-02-02' = {
  name: appInsightsName
  location: location
  kind: 'web'
  properties: {
    Application_Type: 'web'
    WorkspaceResourceId: logAnalytics.id
    IngestionMode: 'LogAnalytics'
  }
}

// ─── Azure Container Registry ───────────────────────────────────────────────

resource acr 'Microsoft.ContainerRegistry/registries@2023-11-01-preview' = {
  name: acrName
  location: location
  sku: { name: 'Standard' }
  properties: {
    adminUserEnabled: false // pull auth is via managed identity, not admin creds
  }
}

// ─── Key Vault (RBAC-authorized, not access-policy) ─────────────────────────

resource keyVault 'Microsoft.KeyVault/vaults@2023-07-01' = {
  name: kvName
  location: location
  properties: {
    sku: { family: 'A', name: 'standard' }
    tenantId: subscription().tenantId
    enableRbacAuthorization: true
    enableSoftDelete: true
    softDeleteRetentionInDays: 7
  }
}

// ─── User-assigned managed identity for the vanguard workload ──────────────

resource workloadIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: uamiName
  location: location
}

// ─── AKS cluster: OIDC issuer + Workload Identity enabled ──────────────────

resource aks 'Microsoft.ContainerService/managedClusters@2024-02-01' = {
  name: aksName
  location: location
  identity: {
    type: 'SystemAssigned'
  }
  properties: {
    dnsPrefix: aksName
    oidcIssuerProfile: {
      enabled: true
    }
    securityProfile: {
      workloadIdentity: {
        enabled: true
      }
    }
    agentPoolProfiles: [
      {
        name: 'system'
        count: nodeCount
        vmSize: nodeVmSize
        mode: 'System'
        osType: 'Linux'
      }
    ]
    addonProfiles: {
      omsagent: {
        enabled: true
        config: {
          logAnalyticsWorkspaceResourceID: logAnalytics.id
        }
      }
      azureKeyvaultSecretsProvider: {
        enabled: true // installs the Secrets Store CSI driver + Key Vault provider
        config: {
          enableSecretRotation: 'true'
        }
      }
    }
  }
}

// ─── Federated identity credential: AKS OIDC issuer ↔ workload identity ────
// This is the whole point of Workload Identity — the vanguard pod
// exchanges a Kubernetes-issued, namespace/ServiceAccount-scoped token for
// an Azure AD token, with no client secret ever stored anywhere.

resource federatedCredential 'Microsoft.ManagedIdentity/userAssignedIdentities/federatedIdentityCredentials@2023-01-31' = {
  parent: workloadIdentity
  name: 'vanguard-control-plane'
  properties: {
    issuer: aks.properties.oidcIssuerProfile.issuerURL
    subject: 'system:serviceaccount:${vanguardNamespace}:${vanguardServiceAccount}'
    audiences: ['api://AzureADTokenExchange']
  }
}

// ─── Role assignments ───────────────────────────────────────────────────────

// AKS kubelet identity → AcrPull (so nodes can pull the vanguard image with
// no imagePullSecret).
resource acrPullRoleAssignment 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(acr.id, aks.id, 'AcrPull')
  scope: acr
  properties: {
    principalId: aks.properties.identityProfile.kubeletidentity.objectId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/roleDefinitions',
      '7f951dda-4ed3-4680-a7ca-43fe172d538d' // AcrPull
    )
  }
}

// Vanguard's workload identity → Key Vault Secrets User (read-only; the
// control plane never needs to write/rotate its own secrets from in-process
// code).
resource kvSecretsUserRoleAssignment 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(keyVault.id, workloadIdentity.id, 'KeyVaultSecretsUser')
  scope: keyVault
  properties: {
    principalId: workloadIdentity.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/roleDefinitions',
      '4633458b-17de-408a-b874-0445c86b69e6' // Key Vault Secrets User
    )
  }
}

// Vanguard's workload identity → Monitoring Metrics Publisher (lets the
// AzureMonitorTelemetrySink / OTel exporter push directly if using the
// Azure Monitor OTLP endpoint instead of an App Insights connection string).
resource monitoringPublisherRoleAssignment 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(appInsights.id, workloadIdentity.id, 'MonitoringMetricsPublisher')
  scope: appInsights
  properties: {
    principalId: workloadIdentity.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/roleDefinitions',
      '3913510d-42f4-4e42-8a64-420c390055eb' // Monitoring Metrics Publisher
    )
  }
}

// ─── Outputs consumed by manifests/azure/*.yaml and the CI/CD workflow ─────

output aksName string = aks.name
output acrLoginServer string = acr.properties.loginServer
output keyVaultUri string = keyVault.properties.vaultUri
output workloadIdentityClientId string = workloadIdentity.properties.clientId
output aksOidcIssuerUrl string = aks.properties.oidcIssuerProfile.issuerURL
output appInsightsConnectionString string = appInsights.properties.ConnectionString
output logAnalyticsWorkspaceId string = logAnalytics.id
