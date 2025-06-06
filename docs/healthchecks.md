# Setting Up Healthchecks

## GRPC

We follow the implementation definition that is recommended by kubernetes and GRPC: https://github.com/grpc-ecosystem/grpc-health-probe/

In the provided dockerfile, you can find the healthcheck probe binary at: /usr/local/bin/grpc-health-probe

This allows you to setup a k8s probe like so (Noting, if you use https the address should be 50052):
```
...
          readinessProbe:
            periodSeconds: 10
            failureThreshold: 3
            exec:
              command:
                - /usr/local/bin/grpc-health-probe
                - '-addr=:50051'
            timeoutSeconds: 1
...
```

## HTTP

To-Do
