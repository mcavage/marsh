TAG ?= docker.io/mcavage/marsh-hermes
HERMES_VERSION ?= 0.21.7

.PHONY: publish
publish:
	docker buildx build --platform linux/arm64 --push -f hermes.yaml \
		-t $(TAG):$(HERMES_VERSION) -t $(TAG):stable .
