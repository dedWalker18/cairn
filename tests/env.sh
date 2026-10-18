# Source to point cairn/cairnd at a throwaway sandbox instead of your home.
S=${CAIRN_TEST_DIR:?set CAIRN_TEST_DIR}
export CAIRN_DATA_DIR=$S/data CAIRN_RUNTIME_DIR=$S/run CAIRN_ROOTS=$S/home
export CAIRN_MODELS_DIR=${CAIRN_MODELS_DIR:-$HOME/.local/share/cairn/models}
