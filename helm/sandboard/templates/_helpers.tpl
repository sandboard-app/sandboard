{{/*
Expand the name of the chart.
*/}}
{{- define "sandboard.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Fully qualified app name.
*/}}
{{- define "sandboard.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "sandboard.labels" -}}
helm.sh/chart: {{ include "sandboard.name" . }}-{{ .Chart.Version | replace "+" "_" }}
app.kubernetes.io/name: {{ include "sandboard.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels for the sandboard Deployment
*/}}
{{- define "sandboard.selectorLabels" -}}
app.kubernetes.io/name: {{ include "sandboard.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: board
{{- end }}

{{/*
Selector labels for Postgres
*/}}
{{- define "sandboard.postgres.selectorLabels" -}}
app.kubernetes.io/name: {{ include "sandboard.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: postgres
{{- end }}

{{/*
Secret name holding master key + database URL
*/}}
{{- define "sandboard.secretName" -}}
{{ include "sandboard.fullname" . }}-secrets
{{- end }}

{{/*
Postgres Service DNS name (in-cluster)
*/}}
{{- define "sandboard.postgres.serviceName" -}}
{{ include "sandboard.fullname" . }}-postgres
{{- end }}
