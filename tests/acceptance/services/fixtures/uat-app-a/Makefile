# J6: fill and unfill the services pool with a bounded file (FILL_MB, default 4096)
# in the volume nfx-fill, through whatever engine DOCKER_HOST selects.
FILL_MB ?= 4096
fill:
	docker volume create nfx-fill >/dev/null
	docker run --rm -v nfx-fill:/f -e MB=$(FILL_MB) busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec sh -c 'dd if=/dev/zero of=/f/fill bs=1M count=$$MB 2>&1 | tail -1'
unfill:
	docker volume rm -f nfx-fill
.PHONY: fill unfill
