TAG ?= docker.io/mcavage/marsh-hermes
HERMES_VERSION ?= 0.22.5

.PHONY: publish
publish:
	docker buildx build --platform linux/arm64 --push -f hermes.yaml \
		--build-arg HERMES_VERSION=$(HERMES_VERSION) \
		-t $(TAG):$(HERMES_VERSION) -t $(TAG):stable .
