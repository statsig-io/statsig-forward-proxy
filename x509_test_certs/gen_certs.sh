#!/bin/bash

# Set variables
ROOT_CA_NAME="root_ca"
INTERMEDIATE_CA_NAME="intermediate_ca"
VALIDITY_DAYS=3650  # 10 years

# Check if root and intermediate CAs exist
if [ ! -f "root/certs/${ROOT_CA_NAME}.crt" ] || [ ! -f "intermediate/certs/${INTERMEDIATE_CA_NAME}.crt" ]; then
    echo "Error: Root or Intermediate CA certificates not found. Please run the full script to generate them first."
    exit 1
fi

echo "Using existing Root and Intermediate CAs..."

# Generate Server Certificate
echo "Generating Server Certificate..."
SERVER_NAME="server"
openssl genrsa -out intermediate/private/${SERVER_NAME}.key 2048
chmod 400 intermediate/private/${SERVER_NAME}.key

openssl req -new -sha256 -key intermediate/private/${SERVER_NAME}.key -out intermediate/csr/${SERVER_NAME}.csr -subj "/C=US/ST=State/L=City/O=Organization/OU=Server/CN=${SERVER_NAME}"

# Create a config file for the server certificate with SANs
cat > intermediate/${SERVER_NAME}.cnf <<EOL
[req]
distinguished_name = req_distinguished_name
x509_extensions = v3_req
prompt = no
[req_distinguished_name]
C = US
ST = State
L = City
O = Organization
OU = Server
CN = ${SERVER_NAME}
[v3_req]
basicConstraints = CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth, clientAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = localhost
DNS.2 = statsig-forward-proxy.default.svc.cluster.local
IP.1 = 127.0.0.1
IP.2 = 0.0.0.0
EOL

# Sign Server Certificate with Intermediate CA
openssl x509 -req -in intermediate/csr/${SERVER_NAME}.csr -CA intermediate/certs/${INTERMEDIATE_CA_NAME}.crt -CAkey intermediate/private/${INTERMEDIATE_CA_NAME}.key -CAcreateserial -out intermediate/certs/${SERVER_NAME}.crt -days ${VALIDITY_DAYS} -sha256 -extfile intermediate/${SERVER_NAME}.cnf -extensions v3_req

echo "Server certificate: intermediate/certs/${SERVER_NAME}.crt"
echo "Server private key: intermediate/private/${SERVER_NAME}.key"

# Verify the server certificate
openssl x509 -in intermediate/certs/${SERVER_NAME}.crt -text -noout | grep -A1 "Subject Alternative Name"

# Create a certificate chain file
echo "Creating server certificate chain..."
cat intermediate/certs/${SERVER_NAME}.crt intermediate/certs/${INTERMEDIATE_CA_NAME}.crt root/certs/${ROOT_CA_NAME}.crt > intermediate/certs/${SERVER_NAME}_full_chain.crt
echo "Server certificate full chain: intermediate/certs/${SERVER_NAME}_full_chain.crt"

# Verify the full chain
echo "Verifying the full certificate chain..."
openssl verify -CAfile root/certs/${ROOT_CA_NAME}.crt -untrusted intermediate/certs/${INTERMEDIATE_CA_NAME}.crt intermediate/certs/${SERVER_NAME}.crt

# Generate Client Certificate
echo "Generating Client Certificate..."
CLIENT_NAME="client"
openssl genrsa -out intermediate/private/${CLIENT_NAME}.key 2048
chmod 400 intermediate/private/${CLIENT_NAME}.key

openssl req -new -sha256 -key intermediate/private/${CLIENT_NAME}.key -out intermediate/csr/${CLIENT_NAME}.csr -subj "/C=US/ST=State/L=City/O=Organization/OU=Client/CN=${CLIENT_NAME}"

# Create a config file for the client certificate with SANs
cat > intermediate/${CLIENT_NAME}.cnf <<EOL
[req]
distinguished_name = req_distinguished_name
x509_extensions = v3_req
prompt = no
[req_distinguished_name]
C = US
ST = State
L = City
O = Organization
OU = Client
CN = ${CLIENT_NAME}
[v3_req]
basicConstraints = CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = localhost
IP.1 = 127.0.0.1
EOL

# Sign Client Certificate with Intermediate CA
openssl x509 -req -in intermediate/csr/${CLIENT_NAME}.csr -CA intermediate/certs/${INTERMEDIATE_CA_NAME}.crt -CAkey intermediate/private/${INTERMEDIATE_CA_NAME}.key -CAcreateserial -out intermediate/certs/${CLIENT_NAME}.crt -days ${VALIDITY_DAYS} -sha256 -extfile intermediate/${CLIENT_NAME}.cnf -extensions v3_req

echo "Client certificate: intermediate/certs/${CLIENT_NAME}.crt"
echo "Client private key: intermediate/private/${CLIENT_NAME}.key"

# Verify the client certificate
openssl x509 -in intermediate/certs/${CLIENT_NAME}.crt -text -noout | grep -A1 "Subject Alternative Name"

# Create a client certificate chain file
echo "Creating client certificate chain..."
cat intermediate/certs/${CLIENT_NAME}.crt intermediate/certs/${INTERMEDIATE_CA_NAME}.crt root/certs/${ROOT_CA_NAME}.crt > intermediate/certs/${CLIENT_NAME}_full_chain.crt
echo "Client certificate full chain: intermediate/certs/${CLIENT_NAME}_full_chain.crt"

# Verify the full client chain
echo "Verifying the full client certificate chain..."
openssl verify -CAfile root/certs/${ROOT_CA_NAME}.crt -untrusted intermediate/certs/${INTERMEDIATE_CA_NAME}.crt intermediate/certs/${CLIENT_NAME}.crt