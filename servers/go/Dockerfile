FROM golang:1.25-alpine AS build
WORKDIR /src
COPY go.mod ./
COPY *.go ./
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /slopensearch .

FROM alpine:3.22
COPY --from=build /slopensearch /usr/local/bin/slopensearch
ENV HOST=0.0.0.0 PORT=9200
EXPOSE 9200
USER nobody
ENTRYPOINT ["slopensearch"]
