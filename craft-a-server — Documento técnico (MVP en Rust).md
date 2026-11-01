# craft-a-server — Documento técnico (MVP en Rust)

Oct 29, 2026 · @fer

## Visión y alcance

craft-a-server convierte un mini PC en un servidor de Minecraft que se maneja desde el móvil, sin saber de Linux, Java ni routers.

**Para quién:** grupos de amigos no técnicos que quieren su propio servidor sin pagar hosting.

**Problema:** montar un servidor casero exige Java, puertos, backups y mods a mano. Los paneles actuales (Crafty, Pterodactyl, PufferPanel) asumen que ya sabes administrarlos, y jugar desde fuera sigue pidiendo abrir puertos o depender de un túnel de terceros.

**Lo más cercano que existe:** [Zero Block Bridge](https://github.com/DesvoSoft/ZeroBlockBridge) une gestión de servidores y túnel con playit.gg. craft-a-server se diferencia por la contención de mods, un panel pensado para el móvil y backups verificados.

**Dentro del MVP**

- Servidor Minecraft con Fabric en un contenedor Podman sin privilegios.
- Panel web solo en la red local, con emparejamiento.
- Mods de Modrinth, sin claves de API de terceros.
- Backups automáticos con restauración.
- Túnel con playit.gg, gestionado por craft-a-serverd.
- Instalador de un comando sobre Debian mínimo.

**Fuera del MVP:** imagen USB propia, CurseForge, otros juegos, Bedrock, túnel propio y enlace de modpack para los clientes.

**Principios**

1. Cero pasos manuales para el usuario.
2. Todo lo de fuera es hostil, incluidos los mods: la seguridad viene de contener, no de detectar.
3. Kernel de Linux estándar sin tocar; el valor está en lo que se construye encima.
4. Reutilizar piezas existentes (playit.gg, itzg/minecraft-server) antes que escribir criptografía o enrutado propios.

El nombre craft-a-server es provisional: no he comprobado si el nombre ni el dominio están libres.

## Arquitectura general

El jugador entra por una dirección de playit.gg y el servicio lo reenvía por un túnel que el mini PC abrió hacia fuera: tu router nunca recibe conexiones entrantes.

&#91;embedded content: arquitectura · playit.gg, mini PC y servicios externos\]

El agente, resaltado, es lo único que cruza la frontera de tu casa, y siempre hacia fuera. El panel solo responde en tu WiFi.

**Piezas**

- **craft-a-serverd** (Rust, en el mini PC): gestiona servidores, backups y mods, supervisa el agente de playit.gg y sirve el panel web y la API.
- **Contenedor de Minecraft:** imagen itzg/minecraft-server en Podman sin root, siempre con Fabric. El [ejemplo de mc-router](https://github.com/itzg/mc-router/pull/446/files) usa esa imagen con la variable `EULA`.
- **Agente de playit.gg** (en el mini PC): mantiene la conexión saliente y el túnel; craft-a-serverd lo lanza y lo vigila.
- **playit.gg** (servicio de terceros): da la dirección pública y reenvía el tráfico por el túnel.
- **Modrinth:** único origen de mods, consultado solo desde craft-a-serverd.

**Camino de una conexión**

1. El jugador escribe en Minecraft Java la dirección de playit.gg, una [IP y puerto fijos con un dominio `.craft.playit.gg`](https://playit.gg/support/minecraft-friend-cant-join/).
2. playit.gg recibe la conexión y la reenvía por el túnel que tu agente abrió desde casa.
3. El agente de playit.gg entrega el tráfico al contenedor de Minecraft.
4. Las respuestas vuelven por el mismo camino.

**Decisiones clave** (las marcadas como propuesta son mías y se pueden cambiar)

| Decisión | Elección | Por qué |
| --- | --- | --- |
| Lenguaje | Rust | Binarios estáticos y seguridad de memoria; elegido por ti |
| Servidor | Solo Fabric | Un único loader simplifica mods y pruebas; no ejecuta los plugins de Paper |
| Contenedores | Podman sin root | Sin daemon y sin privilegios; las [guías de endurecimiento](https://blog.openreplay.com/secure-docker-linux/) lo recomiendan |
| Panel | HTML en el servidor con htmx | Sin build de JavaScript; menos piezas (propuesta) |
| Base de datos | SQLite | Un solo archivo, sin servicio aparte (propuesta) |
| Túnel | playit.gg | Gratis para el usuario y sin servidor propio; dependes de un tercero |
| Mods | Solo Modrinth | Sin claves de API de terceros |
| Kernel | El estándar de Debian | Recibe los parches de seguridad de la distribución |

## Workspace en Rust

El repositorio es un único workspace de Cargo con tres crates y una carpeta de despliegue, para compilar todo con un comando y compartir tipos entre servicio y CLI.

| Crate | Tipo | Función | Dependencias previstas (propuesta) |
| --- | --- | --- | --- |
| `craft-a-server-core` | Librería | Tipos compartidos: configuración, errores, modelos de servidor y la interfaz `TunnelProvider` | serde, thiserror |
| `craft-a-serverd` | Binario | Servicio del mini PC: ciclo de vida, backups, mods, panel, API y supervisión del agente de playit.gg | tokio, axum, SQLite, reqwest, tracing |
| `craft-a-server-cli` | Binario | Comando `craft-a-server`: emparejar, estado, diagnóstico | clap |

**Estructura del repositorio**

```
craft-a-server/
├─ Cargo.toml            # workspace
├─ crates/
│  ├─ craft-a-server-core/
│  ├─ craft-a-serverd/        # incluye el módulo del túnel (playit.gg)
│  └─ craft-a-server-cli/
├─ deploy/
│  ├─ install.sh
│  └─ systemd/
└─ docs/
```

**Reglas de código** (propuesta)

- `#![forbid(unsafe_code)]` en todos los crates.
- Sin `unwrap` ni `expect` en rutas de producción; errores tipados con `thiserror`.
- `clippy` y `cargo fmt` obligatorios en CI.
- `cargo audit` y `cargo deny` en CI para vigilar dependencias y licencias.
- Versiones fijadas con `Cargo.lock` y revisión manual de cada dependencia nueva.

## Gestor de servidores (craft-a-serverd)

craft-a-serverd es el único proceso con poder sobre los servidores: crea, arranca, vigila y para contenedores, y es el único que habla con Modrinth y con el agente de playit.gg.

**Ejecución:** servicio systemd bajo un usuario sin privilegios llamado `craft-a-server`, con Podman sin root (propuesta).

**Crear un servidor**

1. El usuario elige la versión de Minecraft y el nombre del servidor, que siempre usa Fabric, y acepta el EULA de Minecraft en el panel.
2. craft-a-serverd crea la carpeta de datos y la fila en SQLite, y genera una contraseña RCON aleatoria que nunca sale del mini PC.
3. Lanza el contenedor con la imagen itzg/minecraft-server, con `EULA` y `TYPE` (siempre Fabric).
4. Espera a que el servidor responda al ping de estado de Minecraft (Server List Ping, implementado en Rust) antes de marcarlo como `running`.

**Java y memoria**

- Se espera que la imagen elija el Java que necesita cada versión de Minecraft; por verificar.
- Memoria del servidor = RAM total menos una reserva para el sistema y craft-a-serverd (propuesta inicial: 1,5 GB), con un máximo configurable. Si quedan menos de 2 GB para el juego, el panel avisa.
- Las flags de JVM tienen un perfil por defecto probado y solo se tocan desde "avanzado".

**Estados:** `creating → stopped → starting → running → stopping`, más `failed` con su motivo. Solo craft-a-serverd cambia el estado, y lo guarda en SQLite.

**Endurecimiento del contenedor** (las [guías de contenedores](https://docs.instructorphp.com/packages/sandbox/4-drivers) coinciden en raíz de solo lectura, sin capacidades y sin escalada de privilegios)

- `--read-only`, con la carpeta de datos como único sitio escribible y `tmpfs` para `/tmp`.
- `--cap-drop=ALL` y `--security-opt no-new-privileges`.
- Límites de memoria, CPU y número de procesos.
- Ningún socket de Podman ni de otro daemon dentro del contenedor.
- Puerto RCON visible solo dentro de la red del contenedor; el puerto de Minecraft, solo para el agente.
- Pendiente de verificar: impedir que el contenedor llegue a la red local (ver Riesgos).

**Podman desde Rust:** la primera versión llama a la CLI de `podman` con un envoltorio tipado y salida JSON; más adelante se puede pasar a la API por socket Unix (propuesta).

**Consola y logs:** logs en vivo por SSE. En el MVP la consola solo admite una lista cerrada de comandos (whitelist, expulsar, `stop`), no texto libre.

## Panel web y API local

El panel solo existe en tu red local, se abre desde el móvil en `craft-a-server.local` y no tiene ninguna ruta hacia internet.

**Acceso**

- **Nombre:** `craft-a-server.local` anunciado por mDNS (avahi en Debian; por verificar en el instalador).
- **Emparejamiento:** al terminar la instalación se muestra un código de un solo uso, que también se genera con `craft-a-server pair`. Al introducirlo en el móvil se crea una sesión (cookie HttpOnly, SameSite=Strict) ligada a ese dispositivo y revocable desde el panel.
- **Defensas del panel:** comprobar `Host` y `Origin` (frena DNS rebinding y peticiones desde otras webs), token CSRF en los formularios, límite de intentos del código y una política CSP estricta.
- **HTTP en la red local** en el MVP. HTTPS local con certificado propio queda para después, porque un certificado autofirmado asusta a un usuario no técnico. Riesgo aceptado: quien esté en tu WiFi podría ver el tráfico del panel.

**Pantallas** (móvil primero)

1. **Inicio:** estado, jugadores conectados, botón de arrancar o parar y dirección para amigos con botón de copiar.
2. **Crear servidor:** tipo, versión, nombre y aceptar el EULA.
3. **Mods:** buscar, instalar, quitar y ver cambios pendientes.
4. **Backups:** lista, crear ahora y restaurar.
5. **Jugadores:** whitelist por nombre y expulsar.
6. **Registros:** logs en vivo.
7. **Ajustes:** dispositivos emparejados, actualizaciones y estado del túnel.

**API**

| Endpoint | Función |
| --- | --- |
| `POST /api/pair` | Canjea el código por una sesión |
| `GET /api/servers` | Lista de servidores y su estado |
| `POST /api/servers` | Crea un servidor |
| `POST /api/servers/{id}/start`, `/stop`, `/restart` | Ciclo de vida |
| `GET /api/servers/{id}/logs` | Logs en vivo (SSE) |
| `GET`, `POST /api/servers/{id}/whitelist` | Lista blanca |
| `GET`, `POST /api/servers/{id}/backups` | Listar y crear backups |
| `POST /api/servers/{id}/backups/{b}/restore` | Restaurar |
| `GET /api/mods/search` | Búsqueda filtrada en Modrinth |
| `POST`, `DELETE /api/servers/{id}/mods` | Instalar o quitar un mod |
| `GET /api/tunnel` | Estado y dirección pública |

Todas exigen sesión, salvo `pair`. La interfaz es HTML generado en el servidor con htmx, sin build de JavaScript (propuesta).

## Mods con Modrinth

Solo se ofrecen mods válidos para servidor, con dependencias resueltas, un snapshot previo y reversión automática si el servidor no arranca.

**Datos que se usan:** cada proyecto de Modrinth indica qué lado necesita (`client_side` y `server_side`), y cada versión lista sus loaders y dependencias. También existe un [endpoint de dependencias por proyecto](https://docs.modrinth.com/api/operations/getdependencies/). Otras herramientas de la comunidad lo usan [sin clave de API por usuario](https://docs.rs/crate/minecli/0.1.0/source/docs/SOURCES.md).

**Flujo de instalación**

1. **Buscar** con filtros: versión de Minecraft del servidor, loader Fabric y `server_side` distinto de `unsupported` (valores exactos por verificar).
2. **Elegir versión:** la más reciente compatible, en canal estable por defecto.
3. **Resolver dependencias:** las obligatorias, de forma recursiva; las opcionales se sugieren; las incompatibles bloquean.
4. **Descargar** a una carpeta de preparación y verificar el hash del archivo (Modrinth publica hashes por archivo; por verificar).
5. **Snapshot** del mundo y de la carpeta de mods.
6. **Mover los archivos y reiniciar** el servidor.
7. **Comprobar la salud:** el servidor debe responder al ping de estado dentro de un tiempo límite.
8. **Si falla**, volver al snapshot, avisar del mod culpable (el último añadido) y dejar el servidor arrancado.

**Reglas**

- Versiones fijadas: ninguna actualización de mods sin confirmación del usuario.
- Antes de instalar, el panel muestra autor, descargas y enlace al proyecto.
- Un mod solo se descarga desde Modrinth, nunca desde una URL que escriba el usuario.
- Peticiones con un User-Agent propio e identificable y caché local; límites de uso por verificar.

**Fuera del MVP:** CurseForge. Exige una clave de API que se concede a cada desarrollador y [no se puede transferir ni redistribuir](https://docs.rs/crate/minecli/0.1.0/source/docs/SOURCES.md), así que no encaja en un proyecto abierto. Tampoco entra el enlace de modpack para que tus amigos instalen los mods de cliente.

## Túnel con playit.gg

craft-a-serverd lanza y vigila el agente de playit.gg y muestra en el panel la dirección pública: tú no pagas ningún relay, pero dependes de un tercero.

**Qué es playit.gg.** Un servicio que permite alojar un servidor sin abrir puertos ni tocar el router. Ofrece [una IP y un puerto fijos con un dominio `.craft.playit.gg`, y su programa es de código abierto](https://playit.gg/support/minecraft-friend-cant-join/), financiado por quien compra dominios personalizados. Sirve aunque tu operador use CGNAT, porque solo necesita conexiones salientes.

**Cómo lo integra craft-a-server.** Otros proyectos ya lo hacen así: [Zero Block Bridge](https://github.com/DesvoSoft/ZeroBlockBridge) descarga el agente en una carpeta propia, guarda su configuración aparte, gestiona su proceso y abre un enlace de reclamo en el navegador.

1. El instalador deja el agente de playit.gg en una carpeta propia, con su versión fijada.
2. craft-a-serverd lo lanza como proceso supervisado, con el usuario sin privilegios `craft-a-server`.
3. El agente genera un enlace de reclamo, que el panel muestra como botón "Conectar con playit.gg".
4. El usuario lo abre una vez. Una [integración de la comunidad](https://modrinth.com/mod/playit-gg) ofrece cuenta de invitado; hay que comprobar si el agente permite lo mismo para ahorrar este paso.
5. craft-a-serverd lee la dirección pública y la muestra en el panel con botón de copiar.

Si el agente cae, craft-a-serverd lo reinicia con esperas crecientes y el panel muestra el estado: conectado, reconectando o sin reclamar.

**Qué implica**

- **Coste:** cero para ti y para el usuario en el plan gratuito. Los límites del plan (túneles, ancho de banda, regiones) están por verificar.
- **Privacidad:** como cualquier relay, playit.gg ve el volumen de tráfico y las IP de los jugadores. Hay que revisar su política antes de publicar.
- **Disponibilidad:** si playit.gg cae o cambia sus condiciones, nadie entra desde fuera; quien esté en tu WiFi sigue jugando.
- **Un paso manual:** reclamar el túnel, salvo que exista cuenta de invitado.

**Intercambiable.** El túnel es una interfaz (`TunnelProvider`) en `craft-a-server-core`, con una sola implementación en el MVP (`PlayitProvider`).

**Plan B, fuera del MVP: relay propio.** Si playit.gg deja de servir, el diseño admite un relay: un VPS con [rathole](https://github.com/rapiz1/rathole) (token por servicio, transporte Noise o TLS) y [mc-router](https://github.com/itzg/mc-router/pull/465/files) (muchos servidores en una IP y un puerto según el nombre pedido), más un servicio de registro que reserve nombres y emita tokens, y un DNS comodín. Exige moderar abusos, limitar el tráfico y contratar un proveedor con mitigación de DDoS, por eso se aplaza.

## Seguridad y modelo de amenazas

Todo el diseño asume que un mod, un jugador o un visitante de internet pueden ser hostiles, y que la defensa real es contener el daño, no detectarlo.

**Precedentes:** Fractureiser repartió malware en mods subidos a Bukkit y CurseForge, incluso inyectado en proyectos que ya existían. BleedingPipe permitía ejecutar comandos en servidores y clientes mediante mods vulnerables ([BleepingComputer](https://www.bleepingcomputer.com/tag/minecraft/)). Log4Shell se podía activar con un solo mensaje de chat ([HotHardware](https://hothardware.com/news/patch-minecraft-server-protect-from-log4j)).

| Amenaza | Qué podría pasar | Defensa |
| --- | --- | --- |
| Mod malicioso | Robo de datos o ejecución de código en el servidor | Contenedor sin root, raíz de solo lectura, sin capacidades, sin acceso a la red local, snapshot y reversión; solo mods de Modrinth |
| Fallo remoto del servidor (tipo Log4Shell) | Código ejecutado por un mensaje de chat o un paquete | Mismo contenedor; versiones recientes por defecto, aviso si el servidor está desactualizado y whitelist activada |
| Panel atacado desde la WiFi o desde una web | Control del servidor | Solo red local, emparejamiento, comprobación de Host y Origin, CSRF y límite de intentos |
| Robo del acceso al túnel | Alguien usa o suplanta tu dirección pública | Guardar el secreto del agente con permisos restringidos y poder revocar el túnel desde playit.gg |
| Caída o cambio de condiciones de playit.gg | Nadie entra desde fuera | La partida en red local sigue; túnel intercambiable y plan B de relay propio |
| Ataque de denegación de servicio | Servidor inaccesible | El tráfico llega a la infraestructura de playit.gg, no a tu IP de casa; su capacidad de mitigación está por verificar |
| Actualización comprometida | Código malicioso en todos los mini PC | Binarios firmados e instalador que verifica la firma; agente de playit.gg con versión fijada |
| Escaneo de puertos desde internet | Descubrir tu IP de casa | Ningún puerto abierto en tu router; los jugadores solo ven la dirección de playit.gg |

**Endurecimiento del sistema**

- Actualizaciones de seguridad automáticas (`unattended-upgrades` de Debian).
- Firewall con entrada denegada, salvo el panel desde la red local.
- SSH desactivado por defecto en el MVP.
- Whitelist de jugadores activada desde la creación del servidor.

**Límite del diseño:** esto es un diseño, no una garantía. Antes de publicarlo para usuarios no técnicos hace falta una revisión de seguridad externa.

## Robustez: backups, apagado y recuperación

Un mundo no debe perderse por un cambio nuestro: antes de tocar nada hay snapshot, y toda operación fallida se revierte sola.

**Backups**

- **Cuándo:** diarios automáticos, antes de instalar o quitar mods, antes de actualizar el servidor y manuales.
- **Cómo:** `save-off` y `save-all flush` por RCON, copia con tar + zstd y `save-on`. La secuencia exacta se prueba en la Fase 1.
- **Retención (propuesta):** 7 diarios, 4 semanales y los 3 últimos previos a un cambio. Aviso cuando quede menos de un 15 % de disco libre.
- **Verificación:** tras cada backup se comprueba que el archivo se lee y contiene `level.dat`; una vez por semana se restaura en una carpeta temporal.
- **Restaurar:** siempre crea antes un backup del estado actual.

**Apagado y arranque**

- **Parada limpia:** guardar el mundo, enviar `stop`, esperar hasta un límite y solo entonces forzar.
- **Bucles de fallo:** tras varios fallos seguidos no reintenta; pasa a `failed` y avisa.
- **Corte de luz:** SQLite en modo WAL, y al arrancar craft-a-serverd concilia lo guardado con el estado real de los contenedores.
- **Disco lleno:** bloquea backups y mods nuevos y avisa antes de que el mundo se corrompa.

**Actualizaciones**

- **Servidor y mods:** snapshot, cambio, comprobación de salud y reversión.
- **craft-a-serverd:** el binario nuevo se coloca junto al anterior, se cambia de forma atómica y se vuelve al anterior si no pasa su comprobación de salud.
- **Base de datos:** migraciones versionadas, con copia previa.

**Observabilidad:** logs estructurados con `tracing` y rotación, estado visible en el panel y un comando `craft-a-server diagnostico` que recoge versión, estado y últimos errores sin datos personales.

## Instalador y sistema base

El MVP se instala con un comando sobre un Debian mínimo con el kernel estándar; la imagen USB propia llega después.

**Sistema base:** Debian mínimo sin escritorio, kernel de la distribución sin tocar, Podman, avahi-daemon (para `craft-a-server.local`), `unattended-upgrades` y un firewall con entrada denegada. La versión exacta de Debian queda por decidir: la estable vigente.

**Instalador** (`deploy/install.sh`), repetible sin romper nada

1. Comprueba sistema, arquitectura, RAM y espacio libre, y avisa si hay menos de 4 GB de RAM (propuesta).
2. Instala las dependencias del sistema.
3. Crea el usuario sin privilegios `craft-a-server`, activa la persistencia de sus servicios (linger) y ajusta los rangos subuid y subgid que necesita Podman sin root.
4. Descarga el binario de craft-a-serverd con su firma, verifica la firma y lo instala. Con el agente de playit.gg hace lo mismo si publica firma o suma de comprobación (por verificar).
5. Crea y activa las unidades systemd.
6. Configura el firewall y las actualizaciones automáticas.
7. Muestra la dirección `http://craft-a-server.local`, el código de emparejamiento y los siguientes pasos.

**Aviso de seguridad:** ejecutar `curl | sh` es mala práctica. La documentación debe ofrecer también descargar, comprobar la firma y ejecutar, y el script debe imprimir lo que va a hacer antes de hacerlo.

**Desinstalar:** `craft-a-server-uninstall` conserva los mundos y los backups salvo que se pida lo contrario.

**Después del MVP:** una imagen generada con mkosi o debootstrap, o con Buildroot o Yocto, sobre el mismo kernel estándar. Se empaqueta cuando el software ya funcione bien sobre Debian, con menos riesgo. Un instalador para Windows queda fuera.

## Pruebas y medición

El éxito se mide en minutos y en pasos manuales, y cada módulo tiene pruebas antes de pasar a la fase siguiente.

| Nivel | Qué cubre | Cómo |
| --- | --- | --- |
| Unitarias | Resolución de dependencias, estados del servidor, ping de estado, plantillas de configuración | `cargo test` con respuestas de Modrinth guardadas como ejemplos |
| Integración | Cliente de Modrinth y supervisión del agente de playit.gg | Servidor HTTP falso, SQLite en memoria y un agente simulado |
| Contenedores | Flags de endurecimiento, límites y restauración de backups | Podman en CI, mundos de prueba, matar el proceso a mitad de un backup |
| Extremo a extremo | Instalar en un Debian limpio y entrar a jugar por la dirección pública | VM con QEMU en CI nocturna; como mínimo, el ping de estado desde fuera |
| Seguridad | Dependencias y panel | `cargo audit`, `cargo deny`, pruebas de Host, Origin y CSRF, y fuzzing del saludo de Minecraft |

**Métricas.** Las metas son propuestas que aún hay que medir, no resultados.

| Métrica | Cómo se mide | Meta inicial |
| --- | --- | --- |
| Minutos de instalar a que un amigo entra | Cronómetro en VM | 10 o menos |
| Pasos manuales tras ejecutar el comando | Recuento | 1 (reclamar el túnel en playit.gg); 0 si hay cuenta de invitado |
| Backups restaurados con éxito | Prueba semanal automática | 100 % |
| Tiempo de reversión tras un mod roto | Cronómetro | 2 minutos o menos |
| RAM en reposo de craft-a-serverd y del agente | Medición con cgroups | A fijar tras la Fase 1 |

## Plan de construcción por fases

Se construye de dentro hacia fuera: primero lo que protege los mundos, luego el panel, los mods, el túnel y el instalador, y cada fase termina con un criterio de salida comprobable.

1. **Fase 0, decisiones y pruebas técnicas.** Workspace y CI, más dos pruebas rápidas: bloquear la red local a un contenedor sin root, y comprobar cómo se comporta el agente de playit.gg (reclamo, cuenta de invitado, reinicios y límites del plan gratuito). *Sale cuando* ambas tienen respuesta escrita.
2. **Fase 1, gestor sin web.** Crear, arrancar, parar y vigilar un servidor Fabric desde la CLI, con backups, restauración y ping de estado. *Sale cuando* restaurar un mundo funciona tras matar el proceso a mitad de un backup.
3. **Fase 2, panel local.** Emparejamiento, crear servidor, logs, whitelist y backups desde el móvil. *Sale cuando* se puede usar sin abrir una terminal.
4. **Fase 3, mods de Modrinth.** Búsqueda filtrada, dependencias, snapshot y reversión. *Sale cuando* un mod roto se revierte solo y una lista de mods populares se instala sin pasos manuales.
5. **Fase 4, túnel con playit.gg.** Supervisión del agente, reclamo desde el panel y dirección pública visible. *Sale cuando* un amigo fuera de tu red entra, incluso con CGNAT, sin tocar el router.
6. **Fase 5, instalador y endurecimiento.** Instalador firmado y revisión de seguridad externa. *Sale cuando* un Debian limpio llega a "mis amigos juegan" en 10 minutos o menos.
7. **Después del MVP:** imagen USB, enlace de modpack para clientes, otros juegos, Bedrock, relay propio (plan B) y CurseForge.

**Cómo trabajar con Claude Code:** una fase cada vez, con pruebas, sin avanzar si falta el criterio de salida, y pidiendo confirmación antes de tocar seguridad o formatos de datos.

## Riesgos y puntos por verificar

Lo más incierto es técnico y está al principio del plan: la red del contenedor y el comportamiento del agente de playit.gg deciden parte del diseño.

| Punto | Por qué importa | Cómo verificarlo |
| --- | --- | --- |
| Bloquear la red local al contenedor sin root | Es la defensa principal contra un mod malicioso | Fase 0: probar reglas de firewall con la red de Podman sin root; es lo más difícil de confirmar |
| Agente de playit.gg | Reclamo, cuenta de invitado, reinicios, límites del plan gratuito y si publica firma o suma de comprobación | Fase 0: probarlo y leer sus condiciones |
| Dependencia de playit.gg | Si cambia sus condiciones o desaparece, nadie entra desde fuera | Mantener el túnel intercambiable y documentado el plan B |
| Privacidad (UE) | playit.gg ve las IP de los jugadores | Revisar su política; revisión legal antes de abrirlo al público |
| API de Modrinth | Límites de uso, User-Agent, hashes y valores exactos de `server_side` | Leer la documentación oficial antes de la Fase 3 |
| Imagen itzg/minecraft-server | Elige Java y memoria; es una dependencia externa | Comprobar licencia, mantenimiento y comportamiento |
| Solo Fabric | No hay plugins de Paper; la compatibilidad depende de mods de Fabric | Confirmar con usuarios reales que basta |
| EULA de Minecraft | Debe aceptarlo el usuario; hay que revisar las normas de Mojang | Leer los términos antes de publicar |
| Licencia del repositorio | Define quién puede reutilizarlo | Decidir antes de publicar |
| Nombre | craft-a-server es provisional | Comprobar que está libre |
| Competencia | Zero Block Bridge ya integra gestión y túnel de playit.gg; usar el mismo túnel acerca los dos proyectos | Diferenciarse en contención de mods, panel móvil, backups verificados y reversión de mods |

**Fuentes.** Las he visto en resultados de búsqueda; no abrí cada página completa, así que conviene comprobar los detalles antes de depender de ellos.

- [Zero Block Bridge](https://github.com/DesvoSoft/ZeroBlockBridge)
- [playit.gg: por qué no entra un amigo](https://playit.gg/support/minecraft-friend-cant-join/)
- [Integración de playit.gg en Minecraft (Modrinth)](https://modrinth.com/mod/playit-gg)
- [mc-router (itzg)](https://github.com/itzg/mc-router/pull/465/files) y [su ejemplo con Docker](https://github.com/itzg/mc-router/pull/446/files)
- [rathole](https://github.com/rapiz1/rathole)
- [Modrinth: dependencias de un proyecto](https://docs.modrinth.com/api/operations/getdependencies/)
- [minecli: notas sobre Modrinth y CurseForge](https://docs.rs/crate/minecli/0.1.0/source/docs/SOURCES.md)
- [Endurecimiento de contenedores](https://docs.instructorphp.com/packages/sandbox/4-drivers) y [Docker y Podman sin root](https://blog.openreplay.com/secure-docker-linux/)
- [Malware en mods de Minecraft (BleepingComputer)](https://www.bleepingcomputer.com/tag/minecraft/)
- [Log4Shell y Minecraft (HotHardware)](https://hothardware.com/news/patch-minecraft-server-protect-from-log4j)
