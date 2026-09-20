// docker buildx bake            -> multi-arch OCI tarball in dist/
// docker buildx bake local      -> native arch, loaded into the daemon
// docker buildx bake release    -> push image + export per-arch binaries to dist/
variable "REGISTRY" { default = "" }
variable "TAG" { default = "latest" }
variable "IMAGE_NAME" { default = "tornas" }
function "image" {
  params = [tag]
  result = REGISTRY == "" ? "${IMAGE_NAME}:${tag}" : "${REGISTRY}/${IMAGE_NAME}:${tag}"
}
group "default" { targets = ["multi"] }
group "release" { targets = ["image", "binaries"] }
target "_common" {
  context    = "."
  dockerfile = "Dockerfile"
}
target "image" {
  inherits  = ["_common"]
  target    = "runtime"
  platforms = ["linux/amd64", "linux/arm64", "linux/arm/v7"]
  tags      = [image(TAG)]
  output    = ["type=registry"]
}
target "binaries" {
  inherits  = ["_common"]
  target    = "export"
  platforms = ["linux/amd64", "linux/arm64", "linux/arm/v7"]
  output    = ["type=local,dest=dist"]
}
target "multi" {
  inherits  = ["_common"]
  target    = "runtime"
  platforms = ["linux/amd64", "linux/arm64", "linux/arm/v7"]
  tags      = [image(TAG)]
  output    = ["type=oci,dest=./dist/tornas-multi.tar"]
}
target "local" {
  inherits = ["_common"]
  target   = "runtime"
  tags     = [image("local")]
  output   = ["type=docker"]
}
target "push" {
  inherits  = ["_common"]
  target    = "runtime"
  platforms = ["linux/amd64", "linux/arm64", "linux/arm/v7"]
  tags      = [image(TAG)]
  output    = ["type=registry"]
}
target "amd64" {
  inherits  = ["_common"]
  target    = "runtime"
  platforms = ["linux/amd64"]
  tags      = [image("amd64")]
  output    = ["type=docker"]
}
target "arm64" {
  inherits  = ["_common"]
  target    = "runtime"
  platforms = ["linux/arm64"]
  tags      = [image("arm64")]
  output    = ["type=docker"]
}
