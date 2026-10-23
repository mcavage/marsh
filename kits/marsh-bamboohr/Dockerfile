# The BambooHR directory tool as a marsh Kit.
FROM docker.io/library/golang:1 AS build
WORKDIR /src
COPY go.mod *.go ./
RUN go test ./... && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags=-s -o /out/bamboohr .

FROM docker.io/mcavage/marsh-shell:latest
COPY --from=build --chmod=0755 /out/bamboohr /usr/local/bin/bamboohr
ENTRYPOINT ["/usr/local/bin/bamboohr"]
