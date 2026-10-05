# Dockstride library 0.2.0 — embedded evaluator nickel-lang-core 0.19.0.
# This file is checked in with the application; evaluation never downloads code.
# Config annotations are the schema. Advanced contracts stay Nickel's responsibility.
let choiceContract = fun choices => std.contract.from_predicate (fun value =>
  std.array.any (fun choice => choice == value) choices) in
let backendContract = choiceContract ["compose", "swarm"] in
let portContract = std.contract.from_predicate (fun value =>
  std.is_number value && value >= 1 && value <= 65535 && std.number.floor value == value) in
let secretSourceContract = std.contract.from_predicate (fun value =>
  std.is_record value &&
  ((std.record.has_field "file" value && std.is_string value.file &&
    std.record.fields value == ["file"] && value.file != "") ||
   (std.record.has_field "external" value && value.external == true &&
    std.record.has_field "name" value && std.is_string value.name && value.name != "" &&
    std.record.fields value == ["external", "name"]))) in
let removeIfPresent = fun key value =>
  if std.record.has_field key value then std.record.remove key value else value in
let normalizeVolumes = fun volumes =>
  if std.is_array volumes then
    std.record.from_array (std.array.map (fun name => {field = name, value = {}}) volumes)
  else volumes in
let swarm = fun canonical =>
  (removeIfPresent "name" canonical) & {
    version | force = "3.8",
    # Checks live behind services so querying metadata never forces the stack.
    services | force =
      let TopLevel = std.contract.from_validator (fun value =>
        if std.record.has_field "include" value then
          'Error {message = "Compose include is not supported by the Swarm adapter; express included services in Nickel"}
        else if std.record.has_field "profiles" value then
          'Error {message = "top-level profiles cannot be rendered for Swarm"}
        else 'Ok) in
      let checked | TopLevel = canonical in
      std.record.map (fun name service =>
        let Compatible = std.contract.from_validator (fun value =>
          let forbidden = std.array.filter (fun key => std.record.has_field key value)
            ["links", "network_mode", "privileged", "devices", "pid", "ipc", "uts", "pull_policy"] in
          if std.array.length forbidden > 0 then
            'Error {message = "services.%{name}.%{std.array.at 0 forbidden} is not supported by the Swarm adapter; use Docker-native deploy fields or a Compose environment"}
          else if !(std.record.has_field "image" value && std.is_string value.image) then
            'Error {message = "services.%{name}.image is required for Swarm; Swarm does not build images"}
          else 'Ok) in
        let checked | Compatible = service in
        checked |> removeIfPresent "build" |> removeIfPresent "develop"
          |> removeIfPresent "depends_on" |> removeIfPresent "profiles"
          |> removeIfPresent "container_name" |> removeIfPresent "restart")
        checked.services,
  } in
{
  libraryVersion = "0.2.0",
  evaluatorVersion = "0.19.0",
  Choice = choiceContract,
  Backend = backendContract,
  Port = portContract,
  SecretSource = secretSourceContract,

  GenerateSecret = fun policy =>
    (policy | {bytes | Number, encoding | choiceContract ["hex", "base64"], ..}) & {kind = "generate"},
  PromptSecret = {kind = "prompt"},
  FileSecret = fun sourcePath => {kind = "file", path = sourcePath | String},
  StdinSecret = {kind = "stdin"},

  forEnvironment = fun env => {
    # Names, paths, Docker-native fields and backend choices remain visible.
    image = fun name => "%{env.imagePrefix}/%{name}:latest",
    secretPath = fun name => "/run/secrets/%{name}",
    grantSecrets = fun names => std.array.map (fun name => {source = name, target = name}) names,
    Env = fun values => std.record.map (fun name value => std.string.from value) values,
    Service = fun service => service | {
      image | String | optional,
      build | optional,
      environment | optional,
      healthcheck | optional,
      develop | optional,
      secrets | Array Dyn | optional,
      ..
    },
    ComposeFile = fun input =>
      let canonicalModel = (removeIfPresent "dockstride" input) & {
        volumes | force = if std.record.has_field "volumes" input then normalizeVolumes input.volumes else {},
      } in
      let rendered = if env.backend == "swarm" then swarm canonicalModel else canonicalModel in
      rendered & {
        # commands/setup.defaults/diagnostics remain CLI-only metadata; they never
        # leak into Docker exports or force services during bootstrap discovery.
        dockstride | not_exported = input.dockstride & {
          canonical | not_exported = canonicalModel,
        },
      },
  },
}
