# NatLB - UPnP NAT load balancer for Kubernetes

## What

A Kubernetes "load balancer" leveraging a UPnP-enabled NAT device (common to private home networks) to expose Services to the Internet.

This isn't a true load balancer though, in that all traffic for a given port will ingress through a single node. But you are running this on a home network anyway, so scalability probably isn't your highest concern!

## How

The NatLB controller picks up on changes to Services with `Type=LoadBalancer`, then tries to find a UPnP Internet Gateway Device (IGD) that it can program to expose the requested service ports to the Internet. It then picks up the external IP reported by the IGD and writes that back into Kubernetes.

Currently, all ingress traffic is attracted to the node running the NatLB controller, where the implicit NodePort takes care of delivering the traffic to the right node (via kube-proxy).

## Integration with k3s

1. [Disable the default `servicelb` load balancer](https://rancher.com/docs/k3s/latest/en/networking/#disabling-the-service-lb), e.g. `curl -sfL https://get.k3s.io | sh -s - server --disable servicelb`
2. Create the NatLB namespace: `kubectl create namespace natlb`
3. Deploy NatLB: `kubectl apply -k ./kustomize`

## Configuration

Modern UPnP routers do not allow UPnP port forwarding for port numbers <1024. For such routers, configure a port forwarding manually (using the NodePort allocated to the service) and set the following annotation on the service:

```
natlb.mccorkell.me.uk/port-forward: manual
```
