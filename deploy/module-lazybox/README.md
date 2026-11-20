# Lazybox Runner deployment

This Codefly workspace packages the `lazybox` module. The module deploys
one private Runner from the Linux image built by the
[Runner package](../../packaging/runner/). The claim mounted at
`/var/lib/lazybox` holds the container's home and XDG directories. The
Runner listens on loopback and the module exports no network endpoint.

## Render

1. Set `spec.persistent-loopback.image` in the
   [Runner service declaration](modules/lazybox/services/runner/service.codefly.yaml)
   to the fully qualified `@sha256:` reference of the Runner image. The
   image index must cover the target cell platforms. The empty value
   prevents an unconfigured render from choosing an image implicitly.
2. Declare the target environment and its registry, namespace, and
   GitOps binding in the consuming Codefly workspace. Use a CSI storage
   class that supports `ReadWriteOncePod`. The default claim size is
   20 GiB. Set `storage-class` explicitly if the cluster default is unsuitable.
3. Use the released go-grpc 0.2.0 agent or a compatible later release.
   From the consuming workspace, run:

   ```sh
   codefly doctor workspace --env "$LAZYBOX_ENV" --module lazybox
   codefly deploy gitops render lazybox --env "$LAZYBOX_ENV"
   ```

Review the generated Deployment and claim before publishing through the
Solution's normal GitOps path. Check the immutable image, one replica,
`Recreate` strategy, the private volume, and absence of a Service.

## Smoke test

Use a disposable cell namespace and a fixture repository. Give the operator
permission to resolve the workload and open pod exec in that namespace.

1. Enroll from a Lazybox client with Kubernetes owner-link support:

   ```sh
   lazybox remote add-kubernetes --name pilot --context "$KUBE_CONTEXT" \
     --namespace "$NAMESPACE" --workload "deployment/$RUNNER_DEPLOYMENT" \
     --container "$RUNNER_CONTAINER"
   ```

   Use the Deployment and container names from the rendered workload.
2. Open the fixture repo from the desktop, start a shell, and make an
   uncommitted edit. Drop the connection and verify that reconnect
   returns to the same live shell.
3. Replace the pod. Verify that the pinned Runner identity, worktree edit,
   and saved transcript survived. Start a fresh shell on the new pod.
4. Verify that Kubernetes rejects exec for an unauthorized principal.
   Remove the desktop enrollment using `lazybox remote rm pilot` and
   confirm revocation on the Runner.
