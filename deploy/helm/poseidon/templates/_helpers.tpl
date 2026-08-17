{{/* Common naming + labels. */}}

{{- define "poseidon.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "poseidon.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := include "poseidon.name" . -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "poseidon.labels" -}}
app.kubernetes.io/name: {{ include "poseidon.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "poseidon.selectorLabels" -}}
app.kubernetes.io/name: {{ include "poseidon.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Image ref - tag falls back to the chart appVersion. */}}
{{- define "poseidon.image" -}}
{{- $tag := .Values.image.tag | default .Chart.AppVersion -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/* The Secret name holding the PAT - existing one if given, else the chart's. */}}
{{- define "poseidon.patSecretName" -}}
{{- if .Values.azureDevOps.existingSecret -}}
{{- .Values.azureDevOps.existingSecret -}}
{{- else -}}
{{- printf "%s-pat" (include "poseidon.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "poseidon.patSecretKey" -}}
{{- if .Values.azureDevOps.existingSecret -}}
{{- .Values.azureDevOps.existingSecretKey -}}
{{- else -}}
pat
{{- end -}}
{{- end -}}

{{- define "poseidon.pvcName" -}}
{{- if .Values.persistence.existingClaim -}}
{{- .Values.persistence.existingClaim -}}
{{- else -}}
{{- printf "%s-data" (include "poseidon.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* ── Auth / oauth2-proxy ──────────────────────────────────────────────── */}}

{{/* The oauth2-proxy workload + service name. */}}
{{- define "poseidon.oauth2proxy.fullname" -}}
{{- printf "%s-oauth2-proxy" (include "poseidon.fullname" .) -}}
{{- end -}}

{{- define "poseidon.oauth2proxy.selectorLabels" -}}
app.kubernetes.io/name: {{ include "poseidon.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: oauth2-proxy
{{- end -}}

{{- define "poseidon.oauth2proxy.labels" -}}
{{ include "poseidon.labels" . }}
app.kubernetes.io/component: oauth2-proxy
{{- end -}}

{{/* The selected provider's value sub-map (e.g. .Values.auth.azure). */}}
{{- define "poseidon.providerConfig" -}}
{{- index .Values.auth .Values.auth.provider -}}
{{- end -}}

{{/* Should the chart deploy its own oauth2-proxy? Yes when auth is on, UNLESS an
     Istio deployment points at a pre-registered external ext-authz provider. */}}
{{- define "poseidon.deployProxy" -}}
{{- if .Values.auth.enabled -}}
{{- if and (eq .Values.ingress.type "istio") .Values.ingress.istio.externalProviderName -}}
{{- else -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/* Secret holding the oauth client id/secret: an existing one if the provider
     block names it, else the chart-created oauth2-proxy secret. */}}
{{- define "poseidon.oauthSecretName" -}}
{{- $cfg := include "poseidon.providerConfig" . | fromYaml -}}
{{- if $cfg.existingSecret -}}
{{- $cfg.existingSecret -}}
{{- else -}}
{{- include "poseidon.oauth2proxy.fullname" . -}}
{{- end -}}
{{- end -}}

{{- define "poseidon.oauthSecretClientIdKey" -}}
{{- $cfg := include "poseidon.providerConfig" . | fromYaml -}}
{{- if $cfg.existingSecret -}}
{{- $cfg.existingSecretKeys.clientId -}}
{{- else -}}
client-id
{{- end -}}
{{- end -}}

{{- define "poseidon.oauthSecretClientSecretKey" -}}
{{- $cfg := include "poseidon.providerConfig" . | fromYaml -}}
{{- if $cfg.existingSecret -}}
{{- $cfg.existingSecretKeys.clientSecret -}}
{{- else -}}
client-secret
{{- end -}}
{{- end -}}

{{/* Cookie-secret Secret name: an existing one if named, else chart-created. */}}
{{- define "poseidon.cookieSecretName" -}}
{{- if .Values.auth.cookieExistingSecret -}}
{{- .Values.auth.cookieExistingSecret -}}
{{- else -}}
{{- printf "%s-cookie" (include "poseidon.oauth2proxy.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* Bundled IdP names. */}}
{{- define "poseidon.dex.fullname" -}}
{{- printf "%s-dex" (include "poseidon.fullname" .) -}}
{{- end -}}
{{- define "poseidon.keycloak.fullname" -}}
{{- printf "%s-keycloak" (include "poseidon.fullname" .) -}}
{{- end -}}

{{/* Bundled Ollama (AI tag suggester). */}}
{{- define "poseidon.ollama.fullname" -}}
{{- printf "%s-ollama" (include "poseidon.fullname" .) -}}
{{- end -}}

{{/* The oauth client secret for a BUNDLED IdP (dex / keycloak). Must be identical
     in the oauth2-proxy secret AND in the IdP's own config, so it's derived
     deterministically from the release (not random) - templates render
     independently and can't share a random value on first install. A bundled IdP
     is a self-hosted POC component, so a derived secret is acceptable; override
     with keycloak.clientSecret for anything real. */}}
{{- define "poseidon.bundledClientSecret" -}}
{{- $cfg := index .Values.auth .Values.auth.provider -}}
{{- if $cfg.clientSecret -}}
{{- $cfg.clientSecret -}}
{{- else -}}
{{- printf "%s-%s-bundled-oauth-client" (include "poseidon.fullname" .) .Values.auth.provider | sha256sum | trunc 40 -}}
{{- end -}}
{{- end -}}

{{/* External URL scheme + base. `auth.cookieSecure` doubles as the https/http
     signal: secure cookies require HTTPS, so cookieSecure=false (a plain-HTTP
     local cluster) also makes the redirect / issuer / login URLs http. */}}
{{- define "poseidon.scheme" -}}
{{- if .Values.auth.cookieSecure -}}https{{- else -}}http{{- end -}}
{{- end -}}
{{- define "poseidon.baseUrl" -}}
{{- printf "%s://%s" (include "poseidon.scheme" .) .Values.ingress.host -}}
{{- end -}}
