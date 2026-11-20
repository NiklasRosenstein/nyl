{{- define "common.labels" -}}
app.kubernetes.io/part-of: platform
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}
