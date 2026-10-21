local_environment(
    name="native",
    compatible_platforms=["linux_x86_64", "linux_arm64"],
)

docker_environment(
    name="python_3_10",
    image="python:3.10.18",
    fallback_environment="native",
)

docker_environment(
    name="python_3_12",
    image="python:3.12.9",
    fallback_environment="native",
)
