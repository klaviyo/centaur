module Api
  module V1
    module Sandbox
      # Per-user access to shared, code-defined workflows. Operator APIs remain separate.
      class WorkflowsController < Api::SandboxBaseController
        class_attribute :client_factory, default: -> { CentaurApiClient.new }

        before_action :require_user
        before_action :require_workflow_capability
        before_action -> { response.headers["Cache-Control"] = "no-store" }

        def index
          render json: { data: WorkflowAccess.readable_by(linked_user).order(:name).map { |record| payload(record) } }
        end

        def show
          render json: { data: payload(readable_workflow) }
        end

        def access
          record = WorkflowAccess.owned_by(linked_user).find_by!(name: params[:id])
          record.replace_access!(params.require(:data).permit(:visibility, reader_oids: [], editor_oids: []))
          render json: { data: payload(record) }
        end

        def run
          record = WorkflowAccess.editable_by(linked_user).find_by!(name: params[:id])
          # Inputs and runtime principals are controlled by the deployed workflow, never by ACL callers.
          schedule = Array(api_client.list_workflow_schedules["schedules"]).find { |entry| entry.is_a?(Hash) && entry["workflow_name"] == record.name }
          result = api_client.create_workflow_run(workflow_name: record.name, input: schedule&.dig("input"))
          render json: { data: result }, status: :accepted
        rescue CentaurApiClient::Error
          render_error(status: :service_unavailable, message: "Workflow could not be started")
        end

        private

        def api_client = self.class.client_factory.call

        def linked_user
          user = current_proxy.principal.console_user
          user if user&.active?
        end

        def require_user
          render_error(status: :forbidden, message: "sandbox principal is not linked to an active Console user") unless linked_user
        end

        def require_workflow_capability
          principal = current_proxy.principal
          allowed = if %w[run access].include?(action_name)
            principal.sandbox_workflows_write_enabled?
          else
            principal.sandbox_workflows_read_enabled?
          end
          render_error(status: :forbidden, message: "workflow sandbox capability is disabled") unless allowed
        end

        def readable_workflow
          WorkflowAccess.readable_by(linked_user).find_by!(name: params[:id])
        end

        def payload(record)
          { name: record.name, access: record.access_payload, can_run: record.editable_by?(linked_user) && current_proxy.principal.sandbox_workflows_write_enabled? }
        end
      end
    end
  end
end
