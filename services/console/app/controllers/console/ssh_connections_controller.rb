class Console::SshConnectionsController < ApplicationController
  layout "console"
  before_action :load_connection

  def show
    redirect_to edit_console_ssh_connection_path unless @connection.persisted?
  end

  def edit
  end

  def update
    attributes = params.require(:ssh_connection).permit(:name, :private_key, :duration_hours).to_h
    attributes.delete("private_key") if attributes["private_key"].blank?
    @connection.assign_attributes(attributes)
    @connection.assign_attributes(destination: nil, username: nil, host_key: nil)
    @connection.revoked_at = nil
    if @connection.save
      redirect_to console_ssh_connection_path, notice: "SSH key saved. Register the public key in your environments."
    else
      render :edit, status: :unprocessable_entity
    end
  end

  def destroy
    @connection.revoke! if @connection.persisted?
    redirect_to console_integrations_path, notice: "SSH disconnected. Remove the public key from your environments if it is no longer needed."
  end

  private

  def load_connection
    @connection = current_user.ssh_connection || current_user.build_ssh_connection(name: "My SSH key", duration_hours: 24)
  end
end
