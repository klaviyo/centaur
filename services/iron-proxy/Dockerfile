# syntax=docker/dockerfile:1.7

ARG IRON_PROXY_IMAGE=340433579684.dkr.ecr.us-east-1.amazonaws.com/klaviyo-iron-proxy:0f49682c4a0301088418f2a7c2c273d86f04a909@sha256:ea2f5750c69caed9cc0c77a8aa1252db6d0307eaf8eeddcd02823dbc524742c5
FROM ${IRON_PROXY_IMAGE}

USER root
RUN --mount=type=cache,target=/var/cache/apk,sharing=locked \
    apk add --update-cache --cache-dir /var/cache/apk curl jq

COPY services/iron-proxy/iron-proxy.yaml /usr/local/share/iron-proxy/proxy.yaml.default
COPY --chmod=755 services/iron-proxy/entrypoint.sh /entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
