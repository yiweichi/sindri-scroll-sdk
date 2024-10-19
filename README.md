# sindri-scroll-sdk

## Temporary Usage Instructions

First, create your `config.json` file from a template
```
cp example.config.json config.json
```
Now edit the config to supply your Sindri API key.

Compile and launch the prover via
```
cargo run --release
```


## Docker Image

You can build the docker image locally via
```
docker build -t sindri-prover -f docker/Dockerfile .
```
