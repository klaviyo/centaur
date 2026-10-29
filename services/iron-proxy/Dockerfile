# syntax=docker/dockerfile:1.7

ARG IRON_PROXY_IMAGE=340433579684.dkr.ecr.us-east-1.amazonaws.com/klaviyo-iron-proxy:4a139e098a200922f5ea1488e51099bf41c01e76@sha256:1a9c458127043a2c5b57a795241c193da07d7485984915c0ddaa0f8415b55656
FROM ${IRON_PROXY_IMAGE}

USER root
RUN --mount=type=cache,target=/var/cache/apk,sharing=locked \
    apk add --update-cache --cache-dir /var/cache/apk curl jq

COPY services/iron-proxy/iron-proxy.yaml /usr/local/share/iron-proxy/proxy.yaml.default
COPY --chmod=755 services/iron-proxy/entrypoint.sh /entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
