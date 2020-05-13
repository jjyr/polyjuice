use ckb_jsonrpc_types::{
    BlockNumber, BlockView, CellOutputWithOutPoint, CellWithStatus, EpochNumber, EpochView,
    HeaderView, OutPoint, Transaction, TransactionWithStatus,
};
use ckb_types::{packed, H256};

macro_rules! jsonrpc {
    (
        $(#[$struct_attr:meta])*
        pub struct $struct_name:ident {$(
            $(#[$attr:meta])*
            pub fn $method:ident(&mut $selff:ident $(, $arg_name:ident: $arg_ty:ty)*)
                -> $return_ty:ty;
        )*}
    ) => (
        $(#[$struct_attr])*
        pub struct $struct_name {
            pub client: reqwest::Client,
            pub url: reqwest::Url,
            pub id: u64,
        }

        impl $struct_name {
            pub fn new(uri: &str) -> Self {
                let url = reqwest::Url::parse(uri).expect("ckb uri, e.g. \"http://127.0.0.1:8114\"");
                $struct_name { url, id: 0, client: reqwest::Client::new(), }
            }

            $(
                $(#[$attr])*
                pub fn $method(&mut $selff $(, $arg_name: $arg_ty)*) -> Result<$return_ty, failure::Error> {
                    let method = String::from(stringify!($method));
                    let params = serialize_parameters!($($arg_name,)*);
                    $selff.id += 1;

                    let mut req_json = serde_json::Map::new();
                    req_json.insert("id".to_owned(), serde_json::json!($selff.id));
                    req_json.insert("jsonrpc".to_owned(), serde_json::json!("2.0"));
                    req_json.insert("method".to_owned(), serde_json::json!(method));
                    req_json.insert("params".to_owned(), params);

                    let mut resp = $selff.client.post($selff.url.clone()).json(&req_json).send()?;
                    let output = resp.json::<ckb_jsonrpc_types::response::Output>()?;
                    match output {
                        ckb_jsonrpc_types::response::Output::Success(success) => {
                            serde_json::from_value(success.result).map_err(Into::into)
                        },
                        ckb_jsonrpc_types::response::Output::Failure(failure) => {
                            Err(failure.error.into())
                        }
                    }
                }
            )*
        }
    )
}

macro_rules! serialize_parameters {
    () => ( serde_json::Value::Null );
    ($($arg_name:ident,)+) => ( serde_json::to_value(($($arg_name,)+))?)
}

jsonrpc!(pub struct RawHttpRpcClient {
    // Chain
    pub fn get_block(&mut self, hash: H256) -> Option<BlockView>;
    pub fn get_block_by_number(&mut self, number: BlockNumber) -> Option<BlockView>;
    pub fn get_block_hash(&mut self, number: BlockNumber) -> Option<H256>;
    pub fn get_cells_by_lock_hash(&mut self, lock_hash: H256, from: BlockNumber, to: BlockNumber) -> Vec<CellOutputWithOutPoint>;
    pub fn get_current_epoch(&mut self) -> EpochView;
    pub fn get_epoch_by_number(&mut self, number: EpochNumber) -> Option<EpochView>;
    pub fn get_header(&mut self, hash: H256) -> Option<HeaderView>;
    pub fn get_header_by_number(&mut self, number: BlockNumber) -> Option<HeaderView>;
    pub fn get_live_cell(&mut self, out_point: OutPoint, with_data: bool) -> CellWithStatus;
    pub fn get_tip_block_number(&mut self) -> BlockNumber;
    pub fn get_tip_header(&mut self) -> HeaderView;
    pub fn get_transaction(&mut self, hash: H256) -> Option<TransactionWithStatus>;

    // Pool
    pub fn send_transaction(&mut self, tx: Transaction) -> H256;
});

pub struct HttpRpcClient {
    url: String,
    client: RawHttpRpcClient,
}

impl HttpRpcClient {
    pub fn new(url: String) -> HttpRpcClient {
        let client = RawHttpRpcClient::new(url.as_str());
        HttpRpcClient { url, client }
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }
    pub fn client(&mut self) -> &mut RawHttpRpcClient {
        &mut self.client
    }
}

impl HttpRpcClient {
    // Chain
    pub fn get_block(&mut self, hash: H256) -> Result<Option<BlockView>, String> {
        self.client.get_block(hash).map_err(|err| err.to_string())
    }
    pub fn get_block_by_number(&mut self, number: u64) -> Result<Option<BlockView>, String> {
        self.client
            .get_block_by_number(BlockNumber::from(number))
            .map_err(|err| err.to_string())
    }
    pub fn get_block_hash(&mut self, number: u64) -> Result<Option<H256>, String> {
        self.client
            .get_block_hash(BlockNumber::from(number))
            .map_err(|err| err.to_string())
    }
    pub fn get_cells_by_lock_hash(
        &mut self,
        lock_hash: H256,
        from: u64,
        to: u64,
    ) -> Result<Vec<CellOutputWithOutPoint>, String> {
        self.client
            .get_cells_by_lock_hash(lock_hash, BlockNumber::from(from), BlockNumber::from(to))
            .map_err(|err| err.to_string())
    }
    pub fn get_current_epoch(&mut self) -> Result<EpochView, String> {
        self.client
            .get_current_epoch()
            .map_err(|err| err.to_string())
    }
    pub fn get_epoch_by_number(&mut self, number: u64) -> Result<Option<EpochView>, String> {
        self.client
            .get_epoch_by_number(EpochNumber::from(number))
            .map_err(|err| err.to_string())
    }
    pub fn get_header(&mut self, hash: H256) -> Result<Option<HeaderView>, String> {
        self.client.get_header(hash).map_err(|err| err.to_string())
    }
    pub fn get_header_by_number(&mut self, number: u64) -> Result<Option<HeaderView>, String> {
        self.client
            .get_header_by_number(BlockNumber::from(number))
            .map_err(|err| err.to_string())
    }
    // TODO: Make `cell::CellData` public
    pub fn get_live_cell(
        &mut self,
        out_point: packed::OutPoint,
        with_data: bool,
    ) -> Result<CellWithStatus, String> {
        self.client
            .get_live_cell(out_point.into(), with_data)
            .map_err(|err| err.to_string())
    }
    pub fn get_tip_block_number(&mut self) -> Result<u64, String> {
        self.client
            .get_tip_block_number()
            .map(|number| number.value())
            .map_err(|err| err.to_string())
    }
    pub fn get_tip_header(&mut self) -> Result<HeaderView, String> {
        self.client.get_tip_header().map_err(|err| err.to_string())
    }
    pub fn get_transaction(&mut self, hash: H256) -> Result<Option<TransactionWithStatus>, String> {
        self.client
            .get_transaction(hash)
            .map_err(|err| err.to_string())
    }

    // Pool
    pub fn send_transaction(&mut self, tx: packed::Transaction) -> Result<H256, String> {
        self.client
            .send_transaction(tx.into())
            .map_err(|err| err.to_string())
    }
}