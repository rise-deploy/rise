# Multi-stage build for the e2e workload-identity verifier fixture.
# The app has no third-party Go dependencies, so the build needs no module
# downloads and the final image is a static binary on distroless (which ships
# CA certs, in case RISE_ISSUER is ever https).
FROM golang:1.23-alpine AS build
WORKDIR /src
COPY go.mod ./
COPY main.go ./
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -o /app .

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /app /app
EXPOSE 8080
ENTRYPOINT ["/app"]
