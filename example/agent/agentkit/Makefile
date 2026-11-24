ifneq ($(wildcard /opt/go/bin/go),)
export GOROOT := /opt/go
export GOPATH := $(HOME)/code/go
endif

GO := $(if $(GOROOT),$(GOROOT)/bin/go,go)

.PHONY: build
build:
	mkdir -p dist
	CGO_ENABLED=0 GOOS=linux "$(GO)" build -trimpath -ldflags="-s -w" -o dist/agenttomo-agentkit .
