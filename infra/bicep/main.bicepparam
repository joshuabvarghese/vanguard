using 'main.bicep'

param namePrefix = 'vanguard'
param vanguardNamespace = 'vanguard-system'
param vanguardServiceAccount = 'vanguard-operator'
param nodeVmSize = 'Standard_D2s_v5'
param nodeCount = 3
param logRetentionDays = 30
