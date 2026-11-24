# Guia de instalação para iniciantes

Este guia é para quem nunca compilou um programa. Ele leva você do zero até o
VIctory compilado e testado no seu PC. Os comandos são os mesmos do
[README](../../README.pt-BR.md), só que explicados passo a passo.

> **Antes de começar.** O VIctory **ainda não é jogável** (veja a seção
> "Situação" do README). Ele também **não traz nenhum arquivo do jogo** e este
> guia não explica como obtê-los. Para rodar o jogo você precisa dos arquivos
> da sua própria cópia; para compilar e rodar os testes, não precisa de nada
> do jogo.

## 1. Confira se o seu PC atende aos requisitos

| Requisito | Como conferir |
|-----------|---------------|
| Windows 10 ou 11, 64 bits | **Configurações → Sistema → Sobre**: "Tipo de sistema" deve dizer 64 bits |
| Processador com AVX2 | Praticamente todo Intel Core de 4ª geração (2013) em diante e todo AMD Ryzen têm |
| Placa de vídeo com DirectX 12, shader model 6.6 e mesh shaders | Na prática, placas "DirectX 12 Ultimate": NVIDIA RTX 20 ou mais nova, AMD Radeon RX 6000 ou mais nova, Intel Arc. Mantenha o driver de vídeo atualizado |
| Memória suficiente para rodar o jogo | Não há um mínimo oficial, mas hoje o jogo renderiza em 4K, e com 16 GB de RAM e 8 GB na placa de vídeo pode faltar memória (issue [#203](https://github.com/Marvisak/VIctory/issues/203)). Para compilar e rodar os testes, isso não importa |
| Alguns GB livres em disco | A compilação e o cache ocupam bastante espaço |

Os menus funcionam sem mesh shaders; o mundo do jogo precisa deles.

## 2. Instale as ferramentas

Você vai instalar três coisas. Depois de cada instalação, **feche e abra de
novo** o PowerShell, senão ele não encontra os programas novos.

### 2.1 Git

O Git baixa o código do projeto e mantém ele atualizado.

1. Acesse <https://git-scm.com/download/win> e baixe o instalador para Windows.
2. Instale com as opções padrão (é só ir clicando em **Next**).

### 2.2 Rust e as ferramentas de C++ da Microsoft

O Rust é a linguagem do VIctory. No Windows ele precisa também das
ferramentas de compilação de C++ da Microsoft, e o instalador do Rust oferece
instalá-las para você (pelo Visual Studio Community, que é gratuito).

1. Acesse <https://rustup.rs> e baixe o **rustup-init.exe** (64 bits).
2. Execute. Se aparecer a mensagem de que faltam os pré-requisitos do Visual
   Studio, escolha a opção **1** (instalação rápida pelo Visual Studio) e
   aguarde o instalador da Microsoft terminar. Ele pode demorar e pedir para
   reiniciar.
3. De volta à janela do Rust, escolha a opção **1** (instalação padrão).

Se preferir instalar as ferramentas de C++ à mão: baixe o **Build Tools para
Visual Studio** no site da Microsoft e marque a carga de trabalho
**Desenvolvimento para desktop com C++**.

> Não se preocupe com a versão do Rust. O projeto fixa a versão certa no
> arquivo `rust-toolchain.toml`, e o Rust baixa essa versão sozinho na
> primeira vez que você compilar.

### 2.3 Confira se deu certo

Abra um PowerShell novo e rode:

```powershell
git --version
cargo --version
```

Os dois devem mostrar um número de versão. Se aparecer "não é reconhecido como
nome de cmdlet", feche e abra o PowerShell; se continuar, reinstale a
ferramenta que falhou.

## 3. Baixe o código

Escolha uma pasta para o projeto (por exemplo, a sua pasta de usuário) e rode:

```powershell
cd $HOME
git clone https://github.com/Marvisak/VIctory.git
cd VIctory
```

A partir daqui, todos os comandos são rodados **dentro da pasta `VIctory`**.

## 4. Compile e rode os testes

```powershell
cargo test --workspace
```

Na primeira vez isso demora bastante: o Rust baixa a versão certa do
compilador, baixa as bibliotecas usadas pelo projeto e compila tudo. As
próximas vezes são bem mais rápidas.

No final deve aparecer `test result: ok` para cada parte do projeto. Testes
que precisam de arquivos que você não configurou são pulados, e isso é normal.

> **No Windows 10**, o teste `only_loopback_is_reachable` falha com a
> mensagem `winhttp.dll lacks WinHttpGetProxySettingsEx`. É um problema
> conhecido do próprio teste (issue
> [#118](https://github.com/Marvisak/VIctory/issues/118)), não da sua
> instalação: pode seguir em frente.

Agora compile o programa em si:

```powershell
cargo build --release -p victory
```

O resultado fica em `target\release\victory.exe`.

> **Só quer ajudar testando?** Se você parou aqui e os testes passaram, isso
> já é uma informação útil para o projeto, porque até agora ele foi testado em
> pouquíssimas máquinas. Se algo falhar, abra uma issue com a mensagem de erro,
> o seu processador, a sua placa de vídeo e a versão do Windows.

## 5. Configure os caminhos

Esta etapa só é necessária para rodar o jogo.

1. Crie uma pasta **fora** da pasta `VIctory` para os seus arquivos, por
   exemplo `D:\victory-data\game`. O VIctory se recusa a usar caminhos dentro
   do repositório, para que nenhum arquivo do jogo acabe enviado por engano.
2. Crie o seu arquivo de configuração a partir do exemplo:

   ```powershell
   copy victory.example.toml victory.toml
   notepad victory.toml
   ```

3. No Bloco de Notas, troque o valor de `game_dir` pelo caminho da sua pasta e
   salve. As outras linhas podem ficar como estão.
4. Confira se o VIctory entendeu os caminhos:

   ```powershell
   cargo run -p victory -- config
   ```

O `victory.toml` fica fora do controle do Git, então ele nunca é enviado para
o GitHub.

## 6. Rode

```powershell
target\release\victory.exe run D:\victory-data\game\<exe do jogo> --execute
```

Troque o caminho pelo da sua pasta e pelo nome do executável do jogo. A barra
de título da janela mostra o framerate. Sem controle conectado, o teclado
funciona como controle (a tabela de teclas está no README).

> **Atenção.** Hoje, ao entrar no mundo do jogo com uma placa de vídeo de
> verdade, ela pode travar, e o Windows reinicia o driver de vídeo: todos os
> monitores ficam pretos por um instante. Isso é esperado no estado atual do
> projeto e não estraga nada, mas salve o que estiver fazendo em outros
> programas antes.

## 7. Atualize

O projeto muda quase todo dia. Para pegar a versão mais nova:

```powershell
git pull
cargo build --release -p victory
```

## Problemas comuns

| Problema | O que fazer |
|----------|-------------|
| `git` ou `cargo` "não é reconhecido" | Feche e abra o PowerShell. Se continuar, reinstale a ferramenta |
| Erro falando de `link.exe` ou do linker | Faltam as ferramentas de C++: instale o Build Tools com **Desenvolvimento para desktop com C++** (passo 2.2) |
| A compilação parece travada | A primeira compilação demora mesmo. Espere enquanto o processador estiver em uso |
| Só o teste `only_loopback_is_reachable` falha, no Windows 10 | Problema conhecido do teste, não da instalação (issue [#118](https://github.com/Marvisak/VIctory/issues/118)) |
| O jogo fecha logo no início com `0xc000001d` (instrução ilegal) | Acontecia em processadores sem a instrução CLWB (Intel até a geração Broadwell e alguns AMD) e já foi corrigido. Atualize (passo 7) e compile de novo |
| `crashes.log` mostra o erro 1455 do Windows | Falta memória confirmada. Feche outros programas ou aumente o arquivo de paginação do Windows e tente de novo. Veja em **Gerenciador de Tarefas → Desempenho → Memória → Confirmada** |
| Os monitores piscam pretos ao entrar no mundo | É o travamento de GPU descrito no passo 6. Para investigar sem travar, há um modo de diagnóstico que usa o processador no lugar da placa de vídeo; veja "Depuração" no README |
| Onde ficam saves, configurações e o `crashes.log` | Em `%LOCALAPPDATA%\victory\data`. Cole esse caminho na barra de endereço do Explorador de Arquivos |

## Como relatar um problema

Abra uma issue em <https://github.com/Marvisak/VIctory/issues> e inclua:

- o que você fez e o que aconteceu;
- processador, placa de vídeo, versão do driver e versão do Windows;
- o trecho do `crashes.log`, se houver;
- o comando que você rodou.

Nunca anexe arquivos do jogo nem trechos do executável. O projeto não aceita
nenhum material do jogo (veja a [política de assets](asset-policy.md)).

Pode escrever em português se preferir, mas em inglês mais gente consegue
ajudar.
