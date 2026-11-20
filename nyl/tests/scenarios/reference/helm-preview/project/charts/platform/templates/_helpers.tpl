{{- define "platform.fullname" -}}
{{- default .Release.Name .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The image by digest: the repository and the digest the image unit built. */}}
{{- define "platform.image" -}}
{{- printf "%s@%s" .Values.image.repository (required "image.digest is required: the platform deploys its image by digest" .Values.image.digest) -}}
{{- end -}}
