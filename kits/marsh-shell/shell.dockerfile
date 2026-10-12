# marsh shell Kit: the stock DHI shell template plus marsh image repair.
FROM dhi.io/sbx-templates:shell-docker@sha256:ff0fe56f7fbc14a969131c52d8e7233862a4810196fbc0e2d8d88441204081a9
USER root
COPY image-repair /tmp/marsh-image-repair
RUN python3 -I -S /tmp/marsh-image-repair/repair.py \
 && rm -rf /tmp/marsh-image-repair
COPY dhi-notices /usr/local/share/licenses/marsh-dhi
RUN python3 -I -S /usr/local/share/licenses/marsh-dhi/verify.py --stage base --receipt

COPY --chmod=0755 marsh-entrypoint.sh /usr/local/bin/marsh-shell

USER agent
WORKDIR /home/agent/workspace
ENTRYPOINT ["/usr/local/bin/marsh-shell"]
