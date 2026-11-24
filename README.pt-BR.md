# VIctory

> Tradução do [README.md](README.md) em inglês, feita em 2026-11-23. Em caso de
> divergência, vale a versão em inglês.

Roda Grand Theft Auto VI (Xbox Series X|S) nativamente no Windows, a partir de
arquivos fornecidos pelo próprio usuário.

O código x86-64 do jogo roda sem alterações dentro do nosso processo; o
VIctory fornece a plataforma por baixo dele e converte os dados do jogo,
antecipadamente quando possível e no primeiro uso quando necessário
(shaders). Veja [docs/pt-BR/architecture.md](docs/pt-BR/architecture.md).

## Situação (2026-11-23)

**Ainda não é jogável.** O jogo inicia, roda seus menus nos próprios 30 fps e
carrega o mundo, onde a renderização falha.

| Etapa | Situação |
|-------|----------|
| Inicialização, telas do primeiro boot (acessibilidade, idioma, configurações, avisos legais) | funciona, 30 fps |
| Vídeo de introdução com som | funciona |
| Tela inicial, perfil e saves | funciona |
| Carregamento do mundo (cerca de 50 s) | funciona |
| O mundo (introdução da primeira missão) | é alcançado e roda com draws indiretos; o card de título renderiza, a cena seguinte ainda fica escura; 0,5–1,8 s por quadro |

Testado em uma única máquina: Windows 11, AMD RX 6700 XT.

> **Aviso.** Entrar no mundo com uma GPU real pode, no momento, travá-la, e o
> Windows então reinicia o driver de vídeo (todos os monitores ficam pretos
> por um instante). Para investigar mais a fundo, use um build de diagnóstico
> com `VICTORY_D3D12_WARP=1`, que renderiza no adaptador de software.

O que cada parte faz hoje e o que ainda falta:

| Subsistema | Hoje | Falta |
|------------|------|-------|
| Loader | Roda o executável do jogo e suas DLLs no nosso processo; relatórios de crash apontam o código que falhou e ficam em `crashes.log` | – |
| Serviços de runtime | Um usuário offline conectado, licenças, pacotes, saves que sobrevivem a um encerramento forçado em qualquer etapa, caixa de entrada de texto | A fala está muda; a caixa de texto e o relançamento não foram testados ao vivo |
| Rede | Offline por projeto: o jogo não consegue acessar a rede | – |
| Arquivos e streaming | Leitura de arquivos compactados e dados em streaming, decodificação zlib e BCPack em paralelo | O carregamento é limitado pelas escritas na memória do jogo |
| Entrada | Controles XInput, teclado, teclado simulando controle | Vibração dos gatilhos |
| Áudio | Som de menu, vídeo e carregamento; vozes XMA e streams Opus decodificam | O reverb está mudo; resultados do Opus não conferidos com o jogo |
| GPU: menus | Draws, compute e apresentação no D3D12 do host; só a memória do jogo que mudou é copiada | O ritmo dos quadros oscila em uma tela nos builds de release |
| GPU: mundo | Pipelines de mesh, buffers escritos por shaders, tempo de vida de descritores, execução indireta (argumentos limitados), pipelines criados por threads de trabalho e guardados em disco | A maior parte da cena ainda falta (shaders não traduzidos descartam draws); quadros levam segundos; faltam queries reais, depth bounds, HDR e escala |
| Shaders | Código de GPU do console recompilado para DXIL no primeiro uso, em cache no disco. Menus: todos. Mundo: mesh 8.191 de 8.506, vertex/pixel 10.617 de 12.346, compute 1.643 de 2.539 | Atomics de imagem de 64 bits, estágios de geometria mesclados, os shaders do construtor de ray tracing; alguns programas de compute precisam de GPU com waves de 64 lanes |
| Texturas | Layouts em tiles do console e detiling nos modos que o jogo usa | Planos de metadados de render targets |
| Modo Series S | Passa pelos menus até a tela de carregamento | O mundo, como acima |

## Primeiros passos

Nunca compilou um programa? Siga o
[guia de instalação para iniciantes](docs/pt-BR/instalacao.md).

Requisitos: Windows 10/11 x64, CPU com AVX2, GPU Direct3D 12 com shader model
6.6 (e mesh shaders para o mundo), Rust (versão fixada em
`rust-toolchain.toml`).

O jogo também precisa de vários GiB de memória confirmada (commit) disponíveis
no Windows ao iniciar. Confira **Confirmada** em **Gerenciador de Tarefas →
Desempenho → Memória**. Se o `crashes.log` mostrar o erro 1455 do Windows,
feche outros programas ou aumente o tamanho do arquivo de paginação e tente de
novo.

```powershell
cargo xtask install-hooks                # verificação da política de assets a cada commit
copy victory.example.toml victory.toml   # depois defina seus caminhos (fora do repositório)
cargo test --workspace                   # não precisa de dados do jogo
cargo run -p victory -- config           # confere seus caminhos
```

Rodar o jogo:

```powershell
cargo build --release -p victory
target\release\victory.exe run <pasta do jogo>\<exe do jogo> --execute
```

A barra de título da janela mostra o framerate. Saves, configurações e o
`crashes.log` ficam na pasta de dados (padrão `%LOCALAPPDATA%\victory\data`);
dados derivados ficam na pasta de cache.

Entrada: controles XInput. Sem nenhum conectado, o teclado vira um controle
enquanto a janela do jogo estiver em foco:

| Teclas | Controle |
|--------|----------|
| W A S D / I J K L | analógico esquerdo / direito |
| setas | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | bumpers, gatilhos |
| F / H | clique dos analógicos |
| Esc, Tab | Menu, View |

## Comandos

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <argumentos do jogo>]
victory config
```

Logs: `VICTORY_LOG=victory_loader=trace,info` (um build de release registra
info para cima).

## Depuração

Um build de release não tem ferramentas de depuração. Para incluí-las:

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variável | Efeito |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | saída de debug e trace |
| `VICTORY_STACKS=20,60` | a pilha de todas as threads nesses segundos |
| `VICTORY_WATCH=<rva>,...` | registra um u32 do jogo quando ele muda |
| `VICTORY_KEYS=30:Enter,35:X` | pressiona teclas nesses segundos, com ou sem foco |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | salva memória do jogo na pasta de cache em caso de crash |
| `VICTORY_CAPTURE=<dir>` | salva os quadros apresentados (potências de dois, ou `VICTORY_CAPTURE_FRAMES=<lista>`) |
| `VICTORY_DUMP_TARGETS=1` | junto com as duas acima, salva todos os render targets desses quadros |
| `VICTORY_D3D12_DEBUG=1` | camada de debug do host |
| `VICTORY_D3D12_GBV=1` | junto com a acima, validação na GPU (uso incorreto de descritores é reportado em vez de causar falha; lento) |
| `VICTORY_D3D12_DRED=1` | em uma falha de GPU, mostra a alocação atingida e os comandos em execução |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omite draws e dispatches, inclusive indiretos, cujo pipeline contenha um dos hashes de shader de 16 dígitos listados |
| `VICTORY_GPU_TIMING=1` | junto com a acima, cada lista de uma submissão roda e é cronometrada separadamente; listas lentas são registradas com seus comandos mais lentos |
| `VICTORY_D3D12_WARP=1` | dispositivo do host no adaptador de software (lento; não consegue reiniciar o driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | loops de shaders traduzidos param após `n` iterações por lane em vez de 2^20 (com entradas próprias no cache de shaders); se um `n` menor deixar os quadros do mundo bem mais rápidos, são os loops que atingem o limite que causam a lentidão |
| `VICTORY_AUDIO_CAPTURE=<arquivo.wav>` | grava o que o jogo toca (só o nosso processo) |
| `VICTORY_XMA_DUMP=<dir>` | salva o contexto e a entrada das primeiras 32 vozes XMA |

Todo build acrescenta crashes e falhas de GPU do host ao `crashes.log` na
pasta de dados do jogo.

## Licença

GPL-3.0-or-later. Veja [LICENSE](LICENSE).
