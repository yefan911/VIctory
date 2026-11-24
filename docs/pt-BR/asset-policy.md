# Política de assets (regra nº 1)

> Tradução de [docs/asset-policy.md](../asset-policy.md), feita em 2026-11-23.
> Em caso de divergência, vale a versão em inglês.

**Nada do jogo é armazenado neste repositório, nunca.** Tudo é gerado na
máquina do usuário a partir de arquivos fornecidos por ele. O mesmo vale para
outros jogos usados em testes e para qualquer outro material proprietário.

## Não é permitido

Em nenhuma forma (arquivos, literais de string, fixtures, snapshots,
documentação):

* assets: modelos, texturas, shaders, áudio, vídeo, textos, arquivos
  compactados;
* o executável ou qualquer parte dele: seções de dados, strings, tabelas,
  recursos, código além das assinaturas descritas abaixo;
* qualquer coisa derivada deles: assets convertidos, código descompilado ou
  desmontado, dumps de shaders, tabelas extraídas, capturas de tela.

## É permitido

* código nosso que entende formatos;
* identidades de build: BLAKE3 de um executável, GUID/age do CodeView;
* offsets, por build, das funções em que fazemos hook, com nomes escolhidos
  por nós;
* assinaturas de padrões de bytes que localizam funções ou dados no
  executável;
* pequenas constantes de interoperabilidade (números mágicos, valores de enum)
  escritas por nós;
* dados de teste sintéticos gerados pelo nosso próprio código.

## A única exceção

`crates/victory-formats/src/bcpack-tables.bin` (cerca de 78 KB): as tabelas
constantes do formato BCPack (modelos de entropia, tabelas de ranking de
índices, códigos de controle, dicionários por formato). Um stream BCPack não
pode ser decodificado sem exatamente essas tabelas, e elas não podem ser
derivadas; por isso são distribuídas com o VIctory por decisão do dono do
projeto (2026-11-23), registradas como exceção em `xtask/src/policy.rs`. Elas
fazem parte do formato, não são trabalho nosso, e a licença do repositório não
as cobre. Nada mais desse tipo é aceito.

## Onde ficam os dados

Fora do repositório, configurados em `victory.toml`: `game_dir` (o dump do
usuário), `test_titles` e `cache_dir` para tudo que é derivado (padrão
`%LOCALAPPDATA%\victory\cache`).

## Como é garantido

1. O `.gitignore` mantém tipos comuns de dados fora do `git status`.
2. `cargo xtask policy` (executado pelo hook de pre-commit; instale com
   `cargo xtask install-hooks`) é uma lista de permissões: os arquivos precisam
   ter um tipo permitido, ter menos de 512 KiB, não conter dados binários nem
   assinaturas binárias conhecidas, e não ter sequências longas de hex/base64.
   Exceções precisam de uma entrada com justificativa em
   `xtask/src/policy.rs`.
3. Em tempo de execução, tudo que grava dados derivados chama
   `victory_core::ensure_outside_repo`.
4. Testes que precisam de arquivos reais recebem os caminhos por variáveis de
   ambiente e são pulados quando elas não estão definidas. Eles verificam
   propriedades, nunca snapshots.

## Se algo escapar

Um commit de correção não basta. Reescreva o histórico
(`git filter-repo --path <arquivo> --invert-paths`), faça force-push e peça
para todos clonarem de novo.
