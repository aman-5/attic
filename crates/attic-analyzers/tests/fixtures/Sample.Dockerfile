ARG BASE_IMAGE=alpine:3.20
FROM ${BASE_IMAGE} AS builder
# NotReal AlsoFake fake()
ENV APP_HOME=/app PATH=/app/bin:$PATH
COPY src/app.sh /app/app.sh
ADD assets/config.json /app/config.json
HEALTHCHECK CMD ["/app/app.sh", "--check"]
FROM builder AS runner
COPY --from=builder /app/app.sh /usr/local/bin/app.sh
ONBUILD COPY assets/onbuild.txt /tmp/onbuild.txt
