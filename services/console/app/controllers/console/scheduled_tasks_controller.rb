class Console::ScheduledTasksController < ApplicationController
  layout "console"
  before_action :set_task, only: %i[show edit update destroy run access]

  def index
    @tasks = ScheduledTask.readable_by(current_user).order(:name, :id)
    @slack_destination_names = slack_destination_names
  end

  def show
  end

  def access
    raise ActiveRecord::RecordNotFound unless @task.author == current_user

    @task.replace_access!(params.require(:access).permit(:visibility, reader_oids: [], editor_oids: []))
    redirect_to console_scheduled_task_path(@task.oid), notice: "Access saved."
  rescue ActiveRecord::RecordInvalid => e
    redirect_to console_scheduled_task_path(@task.oid), alert: e.message
  end

  def new
    @task = current_user.scheduled_tasks.new(
      cron_expression: ScheduledTask::SCHEDULE_PRESETS.fetch("daily"),
      enabled: true
    )
    prepare_form
  end

  def create
    @task = current_user.scheduled_tasks.new
    if @task.update(task_attributes)
      redirect_to console_scheduled_tasks_path, notice: "Task created."
    else
      prepare_form
      render :new, status: :unprocessable_entity
    end
  end

  def edit
    prepare_form
  end

  def update
    if @task.update(task_attributes)
      redirect_to console_scheduled_tasks_path, notice: "Task saved."
    else
      prepare_form
      render :edit, status: :unprocessable_entity
    end
  end

  def destroy
    raise ActiveRecord::RecordNotFound unless @task.author == current_user

    @task.destroy!
    redirect_to console_scheduled_tasks_path, notice: "Task deleted."
  end

  def run
    unless @task.enabled?
      return redirect_to console_scheduled_tasks_path, alert: "Enable the task before running it."
    end

    ScheduledTaskRunJob.perform_later(@task.id, Time.current.iso8601)
    redirect_to console_scheduled_tasks_path, notice: "Task queued."
  end

  private

  def set_task
    @task = ScheduledTask.readable_by(current_user).find_by_oid!(params[:id])
    if %w[edit update run].include?(action_name) && !@task.editable_by?(current_user)
      raise ActiveRecord::RecordNotFound
    end
  end

  def task_attributes
    attributes = task_params
    delivery_mode = attributes.delete(:delivery_mode)
    schedule_preset = attributes.delete(:schedule_preset)
    custom_days = attributes.delete(:custom_days)
    custom_time = attributes.delete(:custom_time)
    cron_expression = ScheduledTask.cron_for(
      schedule_preset,
      nil,
      custom_days: custom_days,
      custom_time: custom_time
    )
    cron_expression = "invalid custom schedule" if schedule_preset == "custom" && cron_expression.blank?

    attributes.merge(
      delivery_channel: delivery_channel_for(delivery_mode, attributes[:delivery_channel]),
      cron_expression: cron_expression
    )
  end

  def delivery_channel_for(delivery_mode, channel_id)
    return SlackDeliveryPolicy.new(@task&.author || current_user).direct_message_user_id if delivery_mode == "dm"
    return channel_id if delivery_mode == "channel"

    nil
  end

  def task_params
    params.require(:scheduled_task).permit(
      :name,
      :prompt,
      :delivery_mode,
      :delivery_channel,
      :schedule_preset,
      :custom_time,
      :enabled,
      custom_days: []
    )
  end

  def prepare_form
    @slack_dm_user_id = SlackDeliveryPolicy.new(@task&.author || current_user).direct_message_user_id
    submitted_schedule = params[:scheduled_task]
    @schedule_preset = submitted_schedule&.[](:schedule_preset).presence || @task.schedule_preset
    @custom_schedule_days = if submitted_schedule&.key?(:custom_days)
      Array(submitted_schedule[:custom_days])
    else
      @task.custom_schedule_days
    end
    @custom_schedule_time = submitted_schedule&.[](:custom_time).presence || @task.custom_schedule_time
  end

  def slack_destination_names
    names = SlackBotChannel.pluck(:channel_id, :name).to_h { |channel_id, name| [ channel_id, "##{name}" ] }
    @tasks.each do |task|
      user_id = SlackDeliveryPolicy.new(task.author).direct_message_user_id
      next unless task.delivery_channel == user_id

      names[user_id] = "Direct message to you"
    end
    names
  rescue StandardError => e
    Rails.logger.warn("scheduled_task_slack_channels_load_failed error=#{e.class}: #{e.message}")
    {}
  end
end
