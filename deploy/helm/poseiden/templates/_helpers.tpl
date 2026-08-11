{{/* Common naming + labels. */}}

{{- define "poseiden.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "poseiden.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := include "poseiden.name" . -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "poseiden.labels" -}}
app.kubernetes.io/name: {{ include "poseiden.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "poseiden.selectorLabels" -}}
app.kubernetes.io/name: {{ include "poseiden.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Image ref - tag falls back to the chart appVersion. */}}
{{- define "poseiden.image" -}}
{{- $tag := .Values.image.tag | default .Chart.AppVersion -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/* The Secret name holding the PAT - existing one if given, else the chart's. */}}
{{- define "poseiden.patSecretName" -}}
{{- if .Values.azureDevOps.existingSecret -}}
{{- .Values.azureDevOps.existingSecret -}}
{{- else -}}
{{- printf "%s-pat" (include "poseiden.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "poseiden.patSecretKey" -}}
{{- if .Values.azureDevOps.existingSecret -}}
{{- .Values.azureDevOps.existingSecretKey -}}
{{- else -}}
pat
{{- end -}}
{{- end -}}

{{- define "poseiden.pvcName" -}}
{{- if .Values.persistence.existingClaim -}}
{{- .Values.persistence.existingClaim -}}
{{- else -}}
{{- printf "%s-data" (include "poseiden.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* ── Auth / oauth2-proxy ──────────────────────────────────────────────── */}}

{{/* The oauth2-proxy workload + service name. */}}
{{- define "poseiden.oauth2proxy.fullname" -}}
{{- printf "%s-oauth2-proxy" (include "poseiden.fullname" .) -}}
{{- end -}}

{{- define "poseiden.oauth2proxy.selectorLabels" -}}
app.kubernetes.io/name: {{ include "poseiden.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: oauth2-proxy
{{- end -}}

{{- define "poseiden.oauth2proxy.labels" -}}
{{ include "poseiden.labels" . }}
app.kubernetes.io/component: oauth2-proxy
{{- end -}}

{{/* The selected provider's value sub-map (e.g. .Values.auth.azure). */}}
{{- define "poseiden.providerConfig" -}}
{{- index .Values.auth .Values.auth.provider -}}
{{- end -}}

{{/* Should the chart deploy its own oauth2-proxy? Yes when auth is on, UNLESS an
     Istio deployment points at a pre-registered external ext-authz provider. */}}
{{- define "poseiden.deployProxy" -}}
{{- if .Values.auth.enabled -}}
{{- if and (eq .Values.ingress.type "istio") .Values.ingress.istio.externalProviderName -}}
{{- else -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/* Secret holding the oauth client id/secret: an existing one if the provider
     block names it, else the chart-created oauth2-proxy secret. */}}
{{- define "poseiden.oauthSecretName" -}}
{{- $cfg := include "poseiden.providerConfig" . | fromYaml -}}
{{- if $cfg.existingSecret -}}
{{- $cfg.existingSecret -}}
{{- else -}}
{{- include "poseiden.oauth2proxy.fullname" . -}}
{{- end -}}
{{- end -}}

{{- define "poseiden.oauthSecretClientIdKey" -}}
{{- $cfg := include "poseiden.providerConfig" . | fromYaml -}}
{{- if $cfg.existingSecret -}}
{{- $cfg.existingSecretKeys.clientId -}}
{{- else -}}
client-id
{{- end -}}
{{- end -}}

{{- define "poseiden.oauthSecretClientSecretKey" -}}
{{- $cfg := include "poseiden.providerConfig" . | fromYaml -}}
{{- if $cfg.existingSecret -}}
{{- $cfg.existingSecretKeys.clientSecret -}}
{{- else -}}
client-secret
{{- end -}}
{{- end -}}

{{/* Cookie-secret Secret name: an existing one if named, else chart-created. */}}
{{- define "poseiden.cookieSecretName" -}}
{{- if .Values.auth.cookieExistingSecret -}}
{{- .Values.auth.cookieExistingSecret -}}
{{- else -}}
{{- printf "%s-cookie" (include "poseiden.oauth2proxy.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* Bundled IdP names. */}}
{{- define "poseiden.dex.fullname" -}}
{{- printf "%s-dex" (include "poseiden.fullname" .) -}}
{{- end -}}
{{- define "poseiden.keycloak.fullname" -}}
{{- printf "%s-keycloak" (include "poseiden.fullname" .) -}}
{{- end -}}

{{/* Bundled Ollama (AI tag suggester). */}}
{{- define "poseiden.ollama.fullname" -}}
{{- printf "%s-ollama" (include "poseiden.fullname" .) -}}
{{- end -}}

{{/* The oauth client secret for a BUNDLED IdP (dex / keycloak). Must be identical
     in the oauth2-proxy secret AND in the IdP's own config, so it's derived
     deterministically from the release (not random) - templates render
     independently and can't share a random value on first install. A bundled IdP
     is a self-hosted POC component, so a derived secret is acceptable; override
     with keycloak.clientSecret for anything real. */}}
{{- define "poseiden.bundledClientSecret" -}}
{{- $cfg := index .Values.auth .Values.auth.provider -}}
{{- if $cfg.clientSecret -}}
{{- $cfg.clientSecret -}}
{{- else -}}
{{- printf "%s-%s-bundled-oauth-client" (include "poseiden.fullname" .) .Values.auth.provider | sha256sum | trunc 40 -}}
{{- end -}}
{{- end -}}

{{/* External URL scheme + base. `auth.cookieSecure` doubles as the https/http
     signal: secure cookies require HTTPS, so cookieSecure=false (a plain-HTTP
     local cluster) also makes the redirect / issuer / login URLs http. */}}
{{- define "poseiden.scheme" -}}
{{- if .Values.auth.cookieSecure -}}https{{- else -}}http{{- end -}}
{{- end -}}
{{- define "poseiden.baseUrl" -}}
{{- printf "%s://%s" (include "poseiden.scheme" .) .Values.ingress.host -}}
{{- end -}}
