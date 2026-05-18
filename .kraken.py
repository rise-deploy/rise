from kraken.common import buildscript

buildscript(
    index_url="https://jfrog.helsing-dev.ai/artifactory/api/pypi/python-all/simple",
    requirements=["hs-infrastructure-kraken-hs"],
)

from kraken.hs import build_docker_image, docker, general_project_config, gitlab_ci, helm_chart, helsing_project

helsing_project(name="hs-thirdparty-rise", artifactory_project_key="ext", artifactory_url="https://jfrog.helsing-dev.ai")
general_project_config()
gitlab_ci(use_corpnet=True, use_vault=True)
docker(build_on=["release", "snapshot", "playground"])

rise_image = build_docker_image(
    name="docker.rise",
    dockerfile="Dockerfile",
    target="rise",
)

build_docker_image(
    name="docker.rise-builder",
    dockerfile="Dockerfile",
    target="rise-builder",
    image_suffix="builder",
)

helm_chart(docker_build_group=rise_image, helm_dir="helm")
