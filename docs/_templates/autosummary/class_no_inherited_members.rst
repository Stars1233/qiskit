{# This is identical to class.rst, except for the filtering in `set wanted_methods`. -#}

{{ objname | escape | underline }}

.. currentmodule:: {{ module }}

.. autoclass:: {{ objname }}
   :no-members:
   :show-inheritance:

{% block attributes_summary %}
  {% set wanted_attributes = (attributes | reject('in', inherited_members) | list) %}
  {% if wanted_attributes %}
   .. rubric:: Attributes
    {% for item in wanted_attributes %}
   .. autoattribute:: {{ item }}
    {%- endfor %}
  {% endif %}
{% endblock %}

{% block methods_summary %}
  {% set wanted_methods = (methods | reject('in', inherited_members) | reject('==', '__init__') | list) %}
  {% if wanted_methods %}
   .. rubric:: Methods
    {% for item in wanted_methods %}
   .. automethod:: {{ item }}
    {%- endfor %}
  {% endif %}
{% endblock %}
