# Arquitetura

> Tradução de [docs/architecture.md](../architecture.md), atualizada em 2026-11-24.
> Em caso de divergência, vale a versão em inglês.

```
 arquivos do usuário (fora do repo)      VIctory
 ──────────────────────────────────      ─────────────────────────────────────────
 assets do jogo ───────────────────────► victory-pipeline + victory-game
                                         (victory-formats, victory-shader)
                                                    │
                                                    ▼
                                         cache_dir: dados convertidos
                                                    │
 executável do jogo ─► victory-loader ─► código do jogo (x86-64 nativo)
                       prepara, mapeia,     │ imports
                       vincula              ▼
                                         victory-hle ── Windows do host (APIs compartilhadas)
                                            │ D3D12.X
                                            ▼
                                         victory-gpu ─► D3D12 do host
                                            │ shaders na criação do pipeline
                                            ▼
                                         victory-shader ◄─► cache_dir
```

Os shaders são traduzidos quando o jogo cria um pipeline, a partir dos bytes
que ele passa, de onde quer que os tenha carregado (executável, arquivo
compactado, memória). Convertê-los antecipadamente a partir de arquivos serve
só como aquecimento.

## Crates

| Crate | Função |
|-------|--------|
| `victory-core` | configuração, proteção da política de assets, logs |
| `victory-pe` | análise somente leitura de PE32+ e de bibliotecas de importação |
| `victory-loader` | prepara, mapeia, vincula e inicia a imagem do jogo |
| `victory-hle` | implementações das APIs do console, política de repasse ao host |
| `victory-gpu` | objetos D3D12.X, command lists e memória de GPU → D3D12 do host |
| `victory-shader` | containers de shader do Xbox → shaders do host |
| `victory-formats` | layouts de textura da GPU (tamanhos, tiling, detiling), codecs de áudio; planejado: BCPack |
| `victory-pipeline` | trait de conversor, cache endereçado por conteúdo |
| `victory-game` | formatos, identidades de build e hooks específicos de Grand Theft Auto VI |
| `apps/victory` | interface de linha de comando |
| `xtask` | verificação da política de assets, git hooks |
| `tests/fixtures/hle-probe` | um "jogo" substituto (código nosso) para testes do loader |

Tudo, exceto `victory-game`, é de nível de plataforma e pode ser testado com
qualquer jogo de Xbox Series.

## Resolução de imports

Para cada `dll!função` que o jogo importa, nesta ordem:

1. a nossa implementação em `victory-hle::modules` (sempre tem prioridade);
2. a própria função do host, se `HostPolicy` permitir aquela DLL;
3. um export de uma DLL que o jogo traz junto ao executável, mapeada como o
   executável (só DLLs que nenhuma das opções acima atende);
4. um stub gerado que informa o nome do import quando chamado
   (`--unimplemented trap` entra no depurador e aborta; `zero` registra uma
   vez e retorna 0).

`LoadLibrary*` de uma DLL exclusiva do console que implementamos (ex.:
`xgameruntime.dll`) retorna um pseudo-handle; `GetProcAddress` nele encontra
as nossas funções. `LoadLibrary*` de uma DLL do jogo retorna a imagem mapeada,
mapeando-a antes se nada a importar. Por nome, assim como por import, o jogo
só recebe DLLs do host que `HostPolicy` repassa, e `GetProcAddress` em uma
delas retorna a nossa substituição onde a vinculação de imports retornaria.
`QueryApiImpl` entrega os nossos objetos de runtime por id de provedor e de
interface (`victory-hle::modules::xgameruntime` lista as áreas); áreas online
respondem como um console offline. Os saves do jogo são arquivos dentro de
`VICTORY_DATA_DIR`.

O armazenamento do jogo é uma unidade no console, e os jogos montam caminhos
a partir dela. `victory-hle::modules::files` vincula todo import do jogo que
recebe caminhos (kernel32 e o runtime C/C++) e reescreve caminhos de uma
unidade virtual para a pasta do host por trás dela; o armazenamento local
persistente é uma dessas unidades, sobre uma pasta dentro de
`VICTORY_DATA_DIR`. Outros caminhos chegam ao host sem alteração.

O código de um exe acessa seu TLS estático como slot 0; mapeado como DLL ele
recebe outro slot, então o loader redireciona esses acessos
(`victory-loader::tls`). `victory run --execute` instala um relator de crashes
que identifica falhas como `módulo+rva` e desenrola a pilha da thread que
falhou (`victory-loader::crash`).

`victory census` mostra essa classificação sem executar nada (ele não mapeia
DLLs do jogo; `victory run` sem `--execute` mapeia).

## Shaders

`victory_shader::ir::build` transforma um programa nativo em SSA, uma
instrução por instrução de máquina. `victory_shader::structure` então remove o
EXEC: máscaras de lanes viram booleanos por lane, testes de máscara de toda a
wave viram desvios por lane, e instruções vetoriais mantêm uma guarda só onde
uma lane pode alcançá-las com seu bit EXEC desligado.
`victory_shader::resource` rastreia cada descritor e endereço de memória
escalar até o parâmetro raiz de onde vem (tabela, heap, root descriptor ou
constante). Um descritor que um waterfall loop lê da primeira lane
(`v_readfirstlane_b32`) é rastreado até o load ou a escolha por lane que dá a
cada lane o seu próprio descritor (o `v_cndmask_b32` de uma máscara, ou um
if/else em que cada ramo o define: rastreado nos caminhos que as lanes
seguem, a partir da estruturação), e acessado com um índice de heap
não uniforme numa única passada. `victory translate` passa todos os shaders
que encontra (containers, arquivos SGD5) por essas etapas e lista o que eles
ainda não conseguem tratar.
