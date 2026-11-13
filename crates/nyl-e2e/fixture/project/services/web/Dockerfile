# A tiny web server that runs on any node: BusyBox's httpd serving one page.
# Pinned by digest so the build is the same everywhere.
FROM busybox:1.38.0@sha256:a46257e06c82ad85242d0d38077d0a52d9896389ca60a1a59d3463db586e5fa4
COPY index.html /www/index.html
EXPOSE 8080
CMD ["httpd", "-f", "-p", "8080", "-h", "/www"]
