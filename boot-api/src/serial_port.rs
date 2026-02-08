use dropshot::{HttpError, HttpResponseOk, RequestContext, Path, WebsocketUpgrade, WebsocketEndpointResult};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use std::io::{Read, Write};

use crate::{SerialPort, PortIdPath};

pub async fn list_serial_ports(
    _rqctx: RequestContext<()>,
) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError> {
    let ports = serialport::available_ports()
        .map_err(|e| HttpError::for_internal_error(format!("Failed to enumerate serial ports: {}", e)))?;
    
    let serial_ports = ports.iter()
        .filter(|port| {
            // Filter out virtual ttyS* ports - only show USB devices
            match &port.port_type {
                serialport::SerialPortType::UsbPort(_) => true,
                _ => false,
            }
        })
        .map(|port| {
            // Extract a short ID from the path (last component after the final /)
            let id = port.port_name
                .rsplit('/')
                .next()
                .unwrap_or(&port.port_name)
                .to_string();
            
            // Get a friendly name from USB info if available
            let name = match &port.port_type {
                serialport::SerialPortType::UsbPort(info) => {
                    Some(format!("{} ({})", 
                        info.product.as_deref().unwrap_or("USB Serial"),
                        info.serial_number.as_deref().unwrap_or("no serial")))
                }
                _ => None,
            };
            
            SerialPort {
                id,
                path: port.port_name.clone(),
                name,
            }
        }).collect();
    
    Ok(HttpResponseOk(serial_ports))
}

pub fn serial_shell(
    _rqctx: RequestContext<()>,
    path: Path<PortIdPath>,
    upgraded: WebsocketUpgrade,
) -> WebsocketEndpointResult {
    let port_id = path.into_inner().port_id;
    
    upgraded.handle(move |ws_stream| async move {
        // Find the serial port by ID
        let ports = serialport::available_ports()
            .map_err(|e| format!("Failed to enumerate ports: {}", e))?;
        
        let port_info = ports.iter()
            .find(|p| {
                p.port_name.rsplit('/').next().unwrap_or(&p.port_name) == port_id
                    || p.port_name == port_id
            })
            .ok_or_else(|| format!("Port '{}' not found", port_id))?;
        
        // Open serial port
        let mut serial_port = serialport::new(&port_info.port_name, 115200)
            .timeout(std::time::Duration::from_millis(100))
            .open()
            .map_err(|e| format!("Failed to open serial port: {}", e))?;
        
        // Convert to WebSocket stream
        let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
            ws_stream.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        ).await;
        
        let (mut ws_tx, mut ws_rx) = ws_stream.split();
        
        // Create a channel for serial data
        let (serial_tx, mut serial_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        
        // Spawn task to continuously read from serial and send to channel
        let port_name = port_info.port_name.clone();
        let read_handle = tokio::task::spawn_blocking(move || {
            let mut buffer = [0u8; 1024];
            loop {
                match serial_port.read(&mut buffer) {
                    Ok(n) if n > 0 => {
                        // Send data through channel
                        if serial_tx.send(buffer[..n].to_vec()).is_err() {
                            break; // Channel closed, exit task
                        }
                    }
                    Ok(_) => continue,
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(_) => break,
                }
            }
        });
        
        // Handle bidirectional communication
        loop {
            tokio::select! {
                // Read from serial port channel
                Some(data) = serial_rx.recv() => {
                    if ws_tx.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                
                // Read from WebSocket (user input)
                ws_msg = ws_rx.next() => {
                    match ws_msg {
                        Some(Ok(Message::Binary(data))) => {
                            if let Ok(mut port) = serialport::new(&port_name, 115200).open() {
                                let _ = port.write_all(&data);
                            }
                        }
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(mut port) = serialport::new(&port_name, 115200).open() {
                                let _ = port.write_all(text.as_bytes());
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(_)) => break,
                        _ => {}
                    }
                }
            }
        }
        
        // Clean up the read task
        read_handle.abort();
        
        Ok(())
    })
}
